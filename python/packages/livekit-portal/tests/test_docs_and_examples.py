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

"""Keep the examples and the docs' code in step with the API.

Every example must compile, and every name a docs snippet imports from
`livekit.portal` must exist.
"""
from __future__ import annotations

import importlib
import pathlib
import py_compile
import re

import pytest

REPO = pathlib.Path(__file__).resolve().parents[4]
EXAMPLES = sorted(
    p
    for p in (REPO / "examples" / "python").rglob("*.py")
    if not {".venv", "site-packages"} & set(p.parts)
)
DOCS = sorted(REPO.glob("docs/**/*.md")) + [REPO / "README.md"]
IMPORT = re.compile(r"^\s*from (livekit\.portal(?:\.\w+)*) import \(?([^)\n]+)", re.MULTILINE)
PYTHON_BLOCK = re.compile(r"```python\n(.*?)```", re.DOTALL)


def test_there_is_something_to_check():
    assert EXAMPLES and DOCS


@pytest.mark.parametrize("path", EXAMPLES, ids=lambda p: str(p.relative_to(REPO)))
def test_example_compiles(path, tmp_path):
    py_compile.compile(str(path), cfile=str(tmp_path / "out.pyc"), doraise=True)


def _doc_imports():
    for doc in DOCS:
        for block in PYTHON_BLOCK.findall(doc.read_text()):
            for module, names in IMPORT.findall(block):
                for name in names.split(","):
                    name = name.strip().split(" as ")[0].strip()
                    if name and name.isidentifier():
                        yield doc.relative_to(REPO), module, name


@pytest.mark.parametrize("doc, module, name", sorted(set(_doc_imports())), ids=str)
def test_docs_import_real_names(doc, module, name):
    if module == "livekit.portal.recording" and name == "RrdSink":
        pytest.importorskip("rerun")
    assert hasattr(importlib.import_module(module), name), f"{doc}: {module}.{name}"
