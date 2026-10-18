#!/usr/bin/env python3
"""Measure the complete top-level run using the legacy Rust condition-token model.

This is a source metric, not expanded/type-checked control-flow analysis. Every
written cfg branch and unexpanded macro token tree is included. Closures/async
blocks belong to run; nested function definitions do not. Literal/comment,
lifetime and label contents are excluded. For legacy compatibility, an empty
closure's || counts once, as do the historical reader's where/case/catch tokens.
Inside macro token trees the pinned parser splits lifetimes/labels into an
apostrophe and a word (including keyword words). Exclude only that next word,
ignoring intervening comments, within the same token tree; never carry the
apostrophe across another token or into a nested tree. This restores the same
exclusions as structured Rust nodes without dropping following decisions.
Numeric ratification lives in the architecture/story record, never in this CLI.
"""
import argparse
from collections import Counter
import hashlib
from importlib.metadata import PackageNotFoundError, version
import json
from pathlib import Path
import sys

ALGORITHM = "rust-legacy-condition-tokens-v1"
CONFIGURATION = "unexpanded-source-all-cfg"
PINS = {"tree-sitter": "0.25.2", "tree-sitter-rust": "0.24.2"}
CONDITIONS = frozenset({"if", "for", "while", "&&", "||", "?",
                        "catch", "case", "match", "where"})
IGNORED = frozenset({"string_literal", "raw_string_literal", "char_literal",
                     "line_comment", "block_comment", "lifetime", "label"})


class MetricError(Exception):
    """An unavailable or invalid measurement must never become a green gate."""


def parser_and_versions():
    try:
        import tree_sitter
        import tree_sitter_rust
        versions = {package: version(package) for package in PINS}
    except (ImportError, PackageNotFoundError) as error:
        raise MetricError(f"metric tool unavailable: {error}") from error
    if versions != PINS:
        raise MetricError(f"metric configuration mismatch: require {PINS}; found {versions}")
    try:
        return tree_sitter.Parser(tree_sitter.Language(tree_sitter_rust.language())), versions
    except (TypeError, ValueError, RuntimeError) as error:
        raise MetricError(f"metric configuration mismatch: Rust parser initialization: {error}") from error


def measure(source_path):
    try:
        source = source_path.read_bytes()
    except OSError as error:
        raise MetricError(f"metric source unavailable: {error}") from error
    try:
        source.decode("utf-8")
    except UnicodeDecodeError as error:
        raise MetricError(f"Rust parse failed: source is not UTF-8: {error}") from error
    parser, versions = parser_and_versions()
    tree = parser.parse(source)
    if tree.root_node.has_error:
        raise MetricError("Rust parse failed: source contains an error or missing syntax node")
    targets = []
    for node in tree.root_node.named_children:
        if node.type == "function_item":
            name = node.child_by_field_name("name")
            if name is not None and name.text.removeprefix(b"r#") == b"run":
                targets.append(node)
    if not targets:
        raise MetricError("run function missing: require a top-level free function in the supplied source")
    if len(targets) != 1:
        raise MetricError(f"run function ambiguous: found {len(targets)} top-level definitions")
    target = targets[0]
    counts = Counter()
    stack = [target]
    while stack:
        node = stack.pop()
        if node != target and node.type == "function_item":
            continue
        if node.type in IGNORED:
            continue
        if node.type == "closure_parameters" and node.text == b"||":
            counts["||"] += 1
            continue
        if node.type == "token_tree":
            after_apostrophe = False
            for child in node.children:
                if child.type in {"line_comment", "block_comment"}:
                    continue
                if after_apostrophe and child.text.decode("utf-8").removeprefix("r#").isidentifier():
                    after_apostrophe = False
                    continue
                stack.append(child)
                after_apostrophe = child.type == "'"
        elif node.child_count:
            stack.extend(node.children)
        else:
            token = node.text.decode("utf-8")
            if token in CONDITIONS:
                counts[token] += 1
    return {
        "schema": 1,
        "algorithm": ALGORITHM,
        "configuration": CONFIGURATION,
        "tool_versions": versions,
        "tool_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "source": str(source_path),
        "source_sha256": hashlib.sha256(source).hexdigest(),
        "function": "run",
        "function_sha256": hashlib.sha256(source[target.start_byte:target.end_byte]).hexdigest(),
        "start_byte": target.start_byte,
        "end_byte": target.end_byte,
        "start_line": target.start_point.row + 1,
        "end_line": target.end_point.row + 1,
        "ccn": 1 + sum(counts.values()),
        "decisions": dict(sorted(counts.items())),
    }


def main():
    arguments = argparse.ArgumentParser(description=__doc__)
    arguments.add_argument("source", type=Path, help="Rust source containing top-level run")
    args = arguments.parse_args()
    try:
        report = measure(args.source)
    except MetricError as error:
        print(error, file=sys.stderr)
        return 2
    print(json.dumps(report, sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
