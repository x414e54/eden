#!/usr/bin/env python3
# Copyright (c) Facebook, Inc. and its affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import datetime
import shutil
import tempfile
import time
import unittest
from datetime import timedelta
from pathlib import Path
from typing import Optional

from eden.cli.process_finder import BuildInfo, EdenFSProcess
from eden.cli.test.lib.fake_process_finder import FakeProcessFinder


class ProcessFinderTests(unittest.TestCase):
    def setUp(self) -> None:
        self.maxDiff: Optional[int] = None
        self.tmpdir = Path(tempfile.mkdtemp(prefix="eden_test."))
        self.addCleanup(shutil.rmtree, self.tmpdir)
        self.process_finder = FakeProcessFinder(str(self.tmpdir))

    def test_find_edenfs(self) -> None:
        # Add some non-EdenFS processes
        self.process_finder.add_process(pid=1111, uid=99, cmdline=["sleep", "60"])
        self.process_finder.add_process(pid=7868, uid=99, cmdline=["bash"])

        # Add a couple EdenFS processes owned by user 99
        # Set counters to indicate that process 1234 has done a checkout recently.
        build_time_1234 = 0
        start_age_1234 = timedelta(days=1)
        self.process_finder.add_edenfs(
            pid=1234,
            uid=99,
            eden_dir="/home/nobody/eden_dir_1",
            build_time=build_time_1234,
            start_age=start_age_1234,
        )
        build_time_4567 = 1577836800  # 2020-01-01 00:00:00, UTC
        start_age_4567 = timedelta(hours=4)
        self.process_finder.add_edenfs(
            pid=4567,
            uid=99,
            eden_dir="/home/nobody/local/.eden",
            cmdline=["edenfs", "--edenfs"],
            build_time=build_time_4567,
            start_age=start_age_4567,
        )

        # Add an EdenFS processes owned by user 65534
        build_time_9999 = 1576240496  # 2019-12-13 12:34:56 UTC
        start_age_9999 = timedelta(hours=27)
        self.process_finder.add_edenfs(
            pid=9999,
            uid=65534,
            eden_dir="/data/users/nfsnobody/.eden",
            build_time=build_time_9999,
            start_age=start_age_9999,
        )

        # Call get_edenfs_processes() and check the results
        found_processes = {p.pid: p for p in self.process_finder.get_edenfs_processes()}
        found_minus_cmdline = {
            p.pid: p._replace(cmdline=[]) for p in found_processes.values()
        }
        expected_minus_cmdline = {
            1234: EdenFSProcess(
                pid=1234, uid=99, eden_dir=Path("/home/nobody/eden_dir_1"), cmdline=[]
            ),
            4567: EdenFSProcess(
                pid=4567, uid=99, eden_dir=Path("/home/nobody/local/.eden"), cmdline=[]
            ),
            9999: EdenFSProcess(
                pid=9999,
                uid=65534,
                eden_dir=Path("/data/users/nfsnobody/.eden"),
                cmdline=[],
            ),
        }
        self.assertEqual(found_minus_cmdline, expected_minus_cmdline)

        # Check the build info
        self.assertEqual(
            repr(found_processes[1234].get_build_info()), repr(BuildInfo())
        )
        self.assertEqual(
            repr(found_processes[4567].get_build_info()),
            repr(
                BuildInfo(
                    package_name="fb-eden",
                    package_version="20200101",
                    package_release="000000",
                    revision="1" * 40,
                    upstream_revision="1" * 40,
                    build_time=build_time_4567,
                )
            ),
        )
        self.assertEqual(
            repr(found_processes[9999].get_build_info()),
            repr(
                BuildInfo(
                    package_name="fb-eden",
                    package_version="20191213",
                    package_release="123456",
                    revision="1" * 40,
                    upstream_revision="1" * 40,
                    build_time=build_time_9999,
                )
            ),
        )

        # Check the process start times
        self.assert_age_near(
            start_age_1234, self.process_finder.get_process_start_time(1234)
        )
        self.assert_age_near(
            start_age_4567, self.process_finder.get_process_start_time(4567)
        )
        self.assert_age_near(
            start_age_9999, self.process_finder.get_process_start_time(9999)
        )

    def assert_age_near(self, age: timedelta, timestamp: float) -> None:
        now = time.time()
        absolute_age = time.time() - age.total_seconds()
        if absolute_age > (timestamp - 1.0) and absolute_age < (timestamp + 1.0):
            return

        def time_str(ts: float) -> str:
            dt = datetime.datetime.fromtimestamp(ts, tz=datetime.timezone.utc)
            return dt.strftime("%Y-%m-%d %H:%M:%S")

        msg = (
            f"expected timestamp to have approximate age of {age}:\n"
            f"  now:                    {time_str(now)}\n"
            f"  expected age timestamp: {time_str(absolute_age)}\n"
            f"  actual timestamp:       {time_str(timestamp)}\n"
        )
        self.fail(msg)
