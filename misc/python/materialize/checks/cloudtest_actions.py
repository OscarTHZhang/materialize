# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.


from materialize.checks.actions import Action
from materialize.checks.executors import Executor
from materialize.cloudtest.k8s.environmentd import EnvironmentdStatefulSet
from materialize.util import MzVersion


class ReplaceEnvironmentdStatefulSet(Action):
    """Change the image tag of the environmentd stateful set, re-create the definition and replace the existing one."""

    new_tag: str | None

    def __init__(self, new_tag: str | None = None) -> None:
        self.new_tag = new_tag

    def execute(self, e: Executor) -> None:
        new_version = (
            MzVersion.parse_mz(self.new_tag)
            if self.new_tag
            else MzVersion.parse_cargo()
        )
        print(
            f"Replacing environmentd stateful set from version {e.current_mz_version} to version {new_version}"
        )
        mz = e.cloudtest_application()
        stateful_set = [
            resource
            for resource in mz.resources
            if type(resource) == EnvironmentdStatefulSet
        ]
        assert len(stateful_set) == 1
        stateful_set = stateful_set[0]

        stateful_set.tag = self.new_tag
        stateful_set.replace()
        e.current_mz_version = new_version

    def join(self, e: Executor) -> None:
        # execute is blocking already
        pass
