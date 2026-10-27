# Copyright 2026 LiveKit, Inc.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""The `livekit-portal` command."""
from __future__ import annotations

import argparse
import sys
from typing import List, Optional

from .convert import _cli as convert


def main(argv: Optional[List[str]] = None) -> None:
    parser = argparse.ArgumentParser(prog="livekit-portal")
    subparsers = parser.add_subparsers(dest="command", required=True)
    convert.add_parser(subparsers)
    args = parser.parse_args(argv)
    sys.exit(args.run(args))


if __name__ == "__main__":
    main()
