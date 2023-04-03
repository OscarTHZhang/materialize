// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Logging dataflows for events generated by timely dataflow.

use std::any::Any;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use differential_dataflow::collection::AsCollection;
use differential_dataflow::operators::arrange::arrangement::Arrange;
use serde::{Deserialize, Serialize};
use timely::communication::Allocate;
use timely::container::columnation::{CloneRegion, Columnation};
use timely::dataflow::channels::pact::{Exchange, Pipeline};
use timely::dataflow::channels::pushers::Tee;
use timely::dataflow::operators::generic::builder_rc::OperatorBuilder;
use timely::dataflow::operators::{Filter, InputCapability};
use timely::logging::{
    ChannelsEvent, MessagesEvent, OperatesEvent, ParkEvent, ScheduleEvent, ShutdownEvent,
    TimelyEvent,
};
use tracing::error;

use mz_compute_client::logging::LoggingConfig;
use mz_expr::{permutation_for_arrangement, MirScalarExpr};
use mz_ore::cast::CastFrom;
use mz_repr::{datum_list_size, datum_size, Datum, DatumVec, Diff, Row, Timestamp};
use mz_timely_util::buffer::ConsolidateBuffer;
use mz_timely_util::replay::MzReplay;

use crate::logging::{LogVariant, TimelyLog};
use crate::typedefs::{KeysValsHandle, RowSpine};

use super::EventQueue;

/// Constructs the logging dataflow for timely logs.
///
/// Params
/// * `worker`: The Timely worker hosting the log analysis dataflow.
/// * `config`: Logging configuration
/// * `event_queue`: The source to read log events from.
///
/// Returns a map from log variant to a tuple of a trace handle and a dataflow drop token.
pub(super) fn construct<A: Allocate>(
    worker: &mut timely::worker::Worker<A>,
    config: &LoggingConfig,
    event_queue: EventQueue<TimelyEvent>,
) -> BTreeMap<LogVariant, (KeysValsHandle, Rc<dyn Any>)> {
    let logging_interval_ms = std::cmp::max(1, config.interval.as_millis());
    let worker_id = worker.index();
    let peers = worker.peers();

    worker.dataflow_named("Dataflow: timely logging", move |scope| {
        let (mut logs, token) = Some(event_queue.link).mz_replay(
            scope,
            "timely logs",
            config.interval,
            event_queue.activator,
        );

        // If logging is disabled, we still need to install the indexes, but we can leave them
        // empty. We do so by immediately filtering all logs events.
        if !config.enable_logging {
            logs = logs.filter(|_| false);
        }

        // Build a demux operator that splits the replayed event stream up into the separate
        // logging streams.
        let mut demux = OperatorBuilder::new("Timely Logging Demux".to_string(), scope.clone());
        let mut input = demux.new_input(&logs, Pipeline);
        let (mut operates_out, operates) = demux.new_output();
        let (mut channels_out, channels) = demux.new_output();
        let (mut addresses_out, addresses) = demux.new_output();
        let (mut parks_out, parks) = demux.new_output();
        let (mut messages_sent_out, messages_sent) = demux.new_output();
        let (mut messages_received_out, messages_received) = demux.new_output();
        let (mut schedules_duration_out, schedules_duration) = demux.new_output();
        let (mut schedules_histogram_out, schedules_histogram) = demux.new_output();

        let mut demux_state = DemuxState::default();
        let mut demux_buffer = Vec::new();
        demux.build(move |_capability| {
            move |_frontiers| {
                let mut operates = operates_out.activate();
                let mut channels = channels_out.activate();
                let mut addresses = addresses_out.activate();
                let mut parks = parks_out.activate();
                let mut messages_sent = messages_sent_out.activate();
                let mut messages_received = messages_received_out.activate();
                let mut schedules_duration = schedules_duration_out.activate();
                let mut schedules_histogram = schedules_histogram_out.activate();

                let mut output_buffers = DemuxOutput {
                    operates: ConsolidateBuffer::new(&mut operates, 0),
                    channels: ConsolidateBuffer::new(&mut channels, 1),
                    addresses: ConsolidateBuffer::new(&mut addresses, 2),
                    parks: ConsolidateBuffer::new(&mut parks, 3),
                    messages_sent: ConsolidateBuffer::new(&mut messages_sent, 4),
                    messages_received: ConsolidateBuffer::new(&mut messages_received, 5),
                    schedules_duration: ConsolidateBuffer::new(&mut schedules_duration, 6),
                    schedules_histogram: ConsolidateBuffer::new(&mut schedules_histogram, 7),
                };

                input.for_each(|cap, data| {
                    data.swap(&mut demux_buffer);

                    for (time, logger_id, event) in demux_buffer.drain(..) {
                        // We expect the logging infrastructure to not shuffle events between
                        // workers and this code relies on the assumption that each worker handles
                        // its own events.
                        assert_eq!(logger_id, worker_id);
                        if let TimelyEvent::Messages(msg) = &event {
                            match msg.is_send {
                                true => assert_eq!(msg.source, worker_id),
                                false => assert_eq!(msg.target, worker_id),
                            }
                        }

                        DemuxHandler {
                            state: &mut demux_state,
                            output: &mut output_buffers,
                            logging_interval_ms,
                            peers,
                            time,
                            cap: &cap,
                        }
                        .handle(event);
                    }
                });
            }
        });

        // Encode the contents of each logging stream into its expected `Row` format.
        // We pre-arrange the logging streams to force a consolidation and reduce the amount of
        // updates that reach `Row` encoding.
        let operates = operates
            .as_collection()
            .arrange_core::<_, RowSpine<_, _, _, _>>(
                Exchange::new(move |_| u64::cast_from(worker_id)),
                "PreArrange Timely operates",
            )
            .as_collection(move |(id, name), _| {
                Row::pack_slice(&[
                    Datum::UInt64(u64::cast_from(*id)),
                    Datum::UInt64(u64::cast_from(worker_id)),
                    Datum::String(name),
                ])
            });
        let channels = channels
            .as_collection()
            .arrange_core::<_, RowSpine<_, _, _, _>>(
                Exchange::new(move |_| u64::cast_from(worker_id)),
                "PreArrange Timely operates",
            )
            .as_collection(move |datum, ()| {
                let (source_node, source_port) = datum.source;
                let (target_node, target_port) = datum.target;
                Row::pack_slice(&[
                    Datum::UInt64(u64::cast_from(datum.id)),
                    Datum::UInt64(u64::cast_from(worker_id)),
                    Datum::UInt64(u64::cast_from(source_node)),
                    Datum::UInt64(u64::cast_from(source_port)),
                    Datum::UInt64(u64::cast_from(target_node)),
                    Datum::UInt64(u64::cast_from(target_port)),
                ])
            });
        let addresses = addresses
            .as_collection()
            .arrange_core::<_, RowSpine<_, _, _, _>>(
                Exchange::new(move |_| u64::cast_from(worker_id)),
                "PreArrange Timely addresses",
            )
            .as_collection(move |(id, address), _| create_address_row(*id, address, worker_id));
        let parks = parks
            .as_collection()
            .arrange_core::<_, RowSpine<_, _, _, _>>(
                Exchange::new(move |_| u64::cast_from(worker_id)),
                "PreArrange Timely parks",
            )
            .as_collection(move |datum, ()| {
                Row::pack_slice(&[
                    Datum::UInt64(u64::cast_from(worker_id)),
                    Datum::UInt64(u64::try_from(datum.duration_pow).expect("duration too big")),
                    datum
                        .requested_pow
                        .map(|v| Datum::UInt64(v.try_into().expect("requested too big")))
                        .unwrap_or(Datum::Null),
                ])
            });
        let messages_sent = messages_sent
            .as_collection()
            .arrange_core::<_, RowSpine<_, _, _, _>>(
                Exchange::new(move |_| u64::cast_from(worker_id)),
                "PreArrange Timely messages sent",
            )
            .as_collection(move |datum, ()| {
                Row::pack_slice(&[
                    Datum::UInt64(u64::cast_from(datum.channel)),
                    Datum::UInt64(u64::cast_from(worker_id)),
                    Datum::UInt64(u64::cast_from(datum.worker)),
                ])
            });
        let messages_received = messages_received
            .as_collection()
            .arrange_core::<_, RowSpine<_, _, _, _>>(
                Exchange::new(move |_| u64::cast_from(worker_id)),
                "PreArrange Timely messages received",
            )
            .as_collection(move |datum, ()| {
                Row::pack_slice(&[
                    Datum::UInt64(u64::cast_from(datum.channel)),
                    Datum::UInt64(u64::cast_from(datum.worker)),
                    Datum::UInt64(u64::cast_from(worker_id)),
                ])
            });
        let elapsed = schedules_duration
            .as_collection()
            .arrange_core::<_, RowSpine<_, _, _, _>>(
                Exchange::new(move |_| u64::cast_from(worker_id)),
                "PreArrange Timely duration",
            )
            .as_collection(move |operator, _| {
                Row::pack_slice(&[
                    Datum::UInt64(u64::cast_from(*operator)),
                    Datum::UInt64(u64::cast_from(worker_id)),
                ])
            });
        let histogram = schedules_histogram
            .as_collection()
            .arrange_core::<_, RowSpine<_, _, _, _>>(
                Exchange::new(move |_| u64::cast_from(worker_id)),
                "PreArrange Timely histogram",
            )
            .as_collection(move |datum, _| {
                let row = Row::pack_slice(&[
                    Datum::UInt64(u64::cast_from(datum.operator)),
                    Datum::UInt64(u64::cast_from(worker_id)),
                    Datum::UInt64(u64::try_from(datum.duration_pow).expect("duration too big")),
                ]);
                row
            });

        let logs = [
            (LogVariant::Timely(TimelyLog::Operates), operates),
            (LogVariant::Timely(TimelyLog::Channels), channels),
            (LogVariant::Timely(TimelyLog::Elapsed), elapsed),
            (LogVariant::Timely(TimelyLog::Histogram), histogram),
            (LogVariant::Timely(TimelyLog::Addresses), addresses),
            (LogVariant::Timely(TimelyLog::Parks), parks),
            (LogVariant::Timely(TimelyLog::MessagesSent), messages_sent),
            (
                LogVariant::Timely(TimelyLog::MessagesReceived),
                messages_received,
            ),
        ];

        // Build the output arrangements.
        let mut traces = BTreeMap::new();
        for (variant, collection) in logs {
            if config.index_logs.contains_key(&variant) {
                let key = variant.index_by();
                let (_, value) = permutation_for_arrangement(
                    &key.iter()
                        .cloned()
                        .map(MirScalarExpr::Column)
                        .collect::<Vec<_>>(),
                    variant.desc().arity(),
                );
                let trace = collection
                    .map({
                        let mut row_buf = Row::default();
                        let mut datums = DatumVec::new();
                        move |row| {
                            let datums = datums.borrow_with(&row);
                            row_buf.packer().extend(key.iter().map(|k| datums[*k]));
                            let row_key = row_buf.clone();
                            row_buf.packer().extend(value.iter().map(|k| datums[*k]));
                            let row_val = row_buf.clone();
                            (row_key, row_val)
                        }
                    })
                    .arrange_named::<RowSpine<_, _, _, _>>(&format!("ArrangeByKey {:?}", variant))
                    .trace;
                traces.insert(variant.clone(), (trace, Rc::clone(&token)));
            }
        }

        traces
    })
}

fn create_address_row(id: usize, address: &[usize], worker_id: usize) -> Row {
    let id_datum = Datum::UInt64(u64::cast_from(id));
    let worker_datum = Datum::UInt64(u64::cast_from(worker_id));
    // We're collecting into a Vec because we need to iterate over the Datums
    // twice: once for determining the size of the row, then again for pushing
    // them.
    let address_datums: Vec<_> = address
        .iter()
        .map(|i| Datum::UInt64(u64::cast_from(*i)))
        .collect();

    let row_capacity =
        datum_size(&id_datum) + datum_size(&worker_datum) + datum_list_size(&address_datums);

    let mut address_row = Row::with_capacity(row_capacity);
    let mut packer = address_row.packer();
    packer.push(id_datum);
    packer.push(worker_datum);
    packer.push_list(address_datums);

    address_row
}

/// State maintained by the demux operator.
#[derive(Default)]
struct DemuxState {
    /// Information about live operators, indexed by operator ID.
    operators: BTreeMap<usize, OperatesEvent>,
    /// Maps dataflow IDs to channels in the dataflow.
    dataflow_channels: BTreeMap<usize, Vec<ChannelsEvent>>,
    /// Information about the last requested park.
    last_park: Option<Park>,
    /// Maps channel IDs to vectors counting the messages sent to each target worker.
    messages_sent: BTreeMap<usize, Vec<i64>>,
    /// Maps channel IDs to vectors counting the messages received from each source worker.
    messages_received: BTreeMap<usize, Vec<i64>>,
    /// Stores for scheduled operators the time when they were scheduled.
    schedule_starts: BTreeMap<usize, u128>,
    /// Maps operator IDs to a vector recording the (count, elapsed_ns) values in each histogram
    /// bucket.
    schedules_data: BTreeMap<usize, Vec<(isize, i64)>>,
}

struct Park {
    /// Time when the park occurred.
    time_ns: u128,
    /// Requested park time.
    requested: Option<Duration>,
}

type Pusher<D> = Tee<Timestamp, (D, Timestamp, Diff)>;
type OutputBuffer<'a, 'b, D> = ConsolidateBuffer<'a, 'b, Timestamp, D, Diff, Pusher<D>>;

/// Bundled output buffers used by the demux operator.
//
// We use tuples rather than dedicated `*Datum` structs for `operates` and `addresses` to avoid
// having to manually implement `Columnation`. If `Columnation` could be `#[derive]`ed, that
// wouldn't be an issue.
struct DemuxOutput<'a, 'b> {
    operates: OutputBuffer<'a, 'b, (usize, String)>,
    channels: OutputBuffer<'a, 'b, ChannelDatum>,
    addresses: OutputBuffer<'a, 'b, (usize, Vec<usize>)>,
    parks: OutputBuffer<'a, 'b, ParkDatum>,
    messages_sent: OutputBuffer<'a, 'b, MessageDatum>,
    messages_received: OutputBuffer<'a, 'b, MessageDatum>,
    schedules_duration: OutputBuffer<'a, 'b, usize>,
    schedules_histogram: OutputBuffer<'a, 'b, ScheduleHistogramDatum>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
struct ChannelDatum {
    id: usize,
    source: (usize, usize),
    target: (usize, usize),
}

impl Columnation for ChannelDatum {
    type InnerRegion = CloneRegion<Self>;
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
struct ParkDatum {
    duration_pow: u128,
    requested_pow: Option<u128>,
}

impl Columnation for ParkDatum {
    type InnerRegion = CloneRegion<Self>;
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
struct MessageDatum {
    channel: usize,
    worker: usize,
}

impl Columnation for MessageDatum {
    type InnerRegion = CloneRegion<Self>;
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
struct ScheduleHistogramDatum {
    operator: usize,
    duration_pow: u128,
}

impl Columnation for ScheduleHistogramDatum {
    type InnerRegion = CloneRegion<Self>;
}

/// Event handler of the demux operator.
struct DemuxHandler<'a, 'b, 'c> {
    /// State kept by the demux operator.
    state: &'a mut DemuxState,
    /// Demux output buffers.
    output: &'a mut DemuxOutput<'b, 'c>,
    /// The logging interval specifying the time granularity for the updates.
    logging_interval_ms: u128,
    /// The number of timely workers.
    peers: usize,
    /// The current event time.
    time: Duration,
    /// A capability usable for emitting outputs.
    cap: &'a InputCapability<Timestamp>,
}

impl DemuxHandler<'_, '_, '_> {
    /// Return the timestamp associated with the current event, based on the event time and the
    /// logging interval.
    fn ts(&self) -> Timestamp {
        let time_ms = self.time.as_millis();
        let interval = self.logging_interval_ms;
        let rounded = (time_ms / interval + 1) * interval;
        rounded.try_into().expect("must fit")
    }

    /// Handle the given timely event.
    fn handle(&mut self, event: TimelyEvent) {
        use TimelyEvent::*;

        match event {
            Operates(e) => self.handle_operates(e),
            Channels(e) => self.handle_channels(e),
            Shutdown(e) => self.handle_shutdown(e),
            Park(e) => self.handle_park(e),
            Messages(e) => self.handle_messages(e),
            Schedule(e) => self.handle_schedule(e),
            _ => (),
        }
    }

    fn handle_operates(&mut self, event: OperatesEvent) {
        let ts = self.ts();
        let datum = (event.id, event.name.clone());
        self.output.operates.give(self.cap, (datum, ts, 1));

        let datum = (event.id, event.addr.clone());
        self.output.addresses.give(self.cap, (datum, ts, 1));

        self.state.operators.insert(event.id, event);
    }

    fn handle_channels(&mut self, event: ChannelsEvent) {
        let ts = self.ts();
        let datum = ChannelDatum {
            id: event.id,
            source: event.source,
            target: event.target,
        };
        self.output.channels.give(self.cap, (datum, ts, 1));

        let datum = (event.id, event.scope_addr.clone());
        self.output.addresses.give(self.cap, (datum, ts, 1));

        let dataflow_id = event.scope_addr[0];
        self.state
            .dataflow_channels
            .entry(dataflow_id)
            .or_default()
            .push(event);
    }

    fn handle_shutdown(&mut self, event: ShutdownEvent) {
        // Dropped operators should result in a negative record for
        // the `operates` collection, cancelling out the initial
        // operator announcement.
        // Remove logging for this operator.

        let Some(operator) = self.state.operators.remove(&event.id) else {
            error!(operator_id = ?event.id, "missing operator entry at time of shutdown");
            return;
        };

        // Retract operator information.
        let ts = self.ts();
        let datum = (operator.id, operator.name);
        self.output.operates.give(self.cap, (datum, ts, -1));

        // Retract schedules information for the operator
        if let Some(schedules) = self.state.schedules_data.remove(&event.id) {
            for (bucket, (count, elapsed_ns)) in schedules
                .into_iter()
                .enumerate()
                .filter(|(_, (count, _))| *count != 0)
            {
                self.output
                    .schedules_duration
                    .give(self.cap, (event.id, ts, -elapsed_ns));

                let datum = ScheduleHistogramDatum {
                    operator: event.id,
                    duration_pow: 1 << bucket,
                };
                let diff = Diff::cast_from(-count);
                self.output
                    .schedules_histogram
                    .give(self.cap, (datum, ts, diff));
            }
        }

        if operator.addr.len() == 1 {
            let dataflow_id = operator.addr[0];
            self.handle_dataflow_shutdown(dataflow_id);
        }

        let datum = (operator.id, operator.addr);
        self.output.addresses.give(self.cap, (datum, ts, -1));
    }

    fn handle_dataflow_shutdown(&mut self, dataflow_id: usize) {
        // When a dataflow shuts down, we need to retract all its channels.

        let Some(channels) = self.state.dataflow_channels.remove(&dataflow_id) else {
            return;
        };

        let ts = self.ts();
        for channel in channels {
            // Retract channel description.
            let datum = ChannelDatum {
                id: channel.id,
                source: channel.source,
                target: channel.target,
            };
            self.output.channels.give(self.cap, (datum, ts, -1));

            let datum = (channel.id, channel.scope_addr);
            self.output.addresses.give(self.cap, (datum, ts, -1));

            // Retract messages logged for this channel.
            if let Some(sent) = self.state.messages_sent.remove(&channel.id) {
                for (target_worker, count) in sent.iter().enumerate() {
                    let datum = MessageDatum {
                        channel: channel.id,
                        worker: target_worker,
                    };
                    self.output
                        .messages_sent
                        .give(self.cap, (datum, ts, -count));
                }
            }
            if let Some(received) = self.state.messages_received.remove(&channel.id) {
                for (source_worker, count) in received.iter().enumerate() {
                    let datum = MessageDatum {
                        channel: channel.id,
                        worker: source_worker,
                    };
                    self.output
                        .messages_received
                        .give(self.cap, (datum, ts, -count));
                }
            }
        }
    }

    fn handle_park(&mut self, event: ParkEvent) {
        let time_ns = self.time.as_nanos();
        match event {
            ParkEvent::Park(requested) => {
                let park = Park { time_ns, requested };
                let existing = self.state.last_park.replace(park);
                if existing.is_some() {
                    error!("park without a succeeding unpark");
                }
            }
            ParkEvent::Unpark => {
                let Some(park) = self.state.last_park.take() else {
                    error!("unpark without a preceeding park");
                    return;
                };

                let duration_ns = time_ns - park.time_ns;
                let duration_pow = duration_ns.next_power_of_two();
                let requested_pow = park.requested.map(|r| r.as_nanos().next_power_of_two());

                let ts = self.ts();
                let datum = ParkDatum {
                    duration_pow,
                    requested_pow,
                };
                self.output.parks.give(self.cap, (datum, ts, 1));
            }
        }
    }

    fn handle_messages(&mut self, event: MessagesEvent) {
        let ts = self.ts();
        let count = Diff::try_from(event.length).expect("must fit");

        if event.is_send {
            let datum = MessageDatum {
                channel: event.channel,
                worker: event.target,
            };
            self.output.messages_sent.give(self.cap, (datum, ts, count));

            let sent_counts = self
                .state
                .messages_sent
                .entry(event.channel)
                .or_insert_with(|| vec![0; self.peers]);
            sent_counts[event.target] += count;
        } else {
            let datum = MessageDatum {
                channel: event.channel,
                worker: event.source,
            };
            self.output
                .messages_received
                .give(self.cap, (datum, ts, count));

            let received_counts = self
                .state
                .messages_received
                .entry(event.channel)
                .or_insert_with(|| vec![0; self.peers]);
            received_counts[event.source] += count;
        }
    }

    fn handle_schedule(&mut self, event: ScheduleEvent) {
        let time_ns = self.time.as_nanos();

        match event.start_stop {
            timely::logging::StartStop::Start => {
                let existing = self.state.schedule_starts.insert(event.id, time_ns);
                if existing.is_some() {
                    error!(operator_id = ?event.id, "schedule start without succeeding stop");
                }
            }
            timely::logging::StartStop::Stop => {
                let Some(start_time) = self.state.schedule_starts.remove(&event.id) else {
                    error!(operator_id = ?event.id, "schedule stop without preceeding start");
                    return;
                };

                let elapsed_ns = time_ns - start_time;
                let elapsed_diff = Diff::try_from(elapsed_ns).expect("must fit");
                let elapsed_pow = elapsed_ns.next_power_of_two();

                let ts = self.ts();
                let datum = event.id;
                self.output
                    .schedules_duration
                    .give(self.cap, (datum, ts, elapsed_diff));

                let datum = ScheduleHistogramDatum {
                    operator: event.id,
                    duration_pow: elapsed_pow,
                };
                self.output
                    .schedules_histogram
                    .give(self.cap, (datum, ts, 1));

                // Record count and elapsed time for later retraction.
                let index = usize::cast_from(elapsed_pow.trailing_zeros());
                let data = self.state.schedules_data.entry(event.id).or_default();
                grow_vec(data, index);
                let (count, duration) = &mut data[index];
                *count += 1;
                *duration += elapsed_diff;
            }
        }
    }
}

/// Grow the given vector so it fits the given index.
///
/// This does nothing if the vector is already large enough.
fn grow_vec<T>(vec: &mut Vec<T>, index: usize)
where
    T: Clone + Default,
{
    if vec.len() <= index {
        vec.resize(index + 1, Default::default());
    }
}
