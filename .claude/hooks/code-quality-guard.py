#!/usr/bin/env python3
"""
Code Quality Guard — PreToolUse hook for Write/Edit/MultiEdit.

Catches common coding mistakes before they reach the codebase:
1. Copyright in //! (doc comment) instead of // (regular comment)
2. .unwrap() in production code (outside tests)

Exit codes:
  0 — allow
  2 — block with explanation
"""

import json
import os
import re
import sys

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.normpath(os.path.join(SCRIPT_DIR, "..", ".."))


def is_under_repo(file_path):
    try:
        norm = os.path.normpath(os.path.abspath(file_path))
        return norm.startswith(REPO_ROOT + os.sep)
    except (TypeError, ValueError):
        return False


def is_test_file(file_path):
    """Check if the file is a test file or inside a tests directory."""
    norm = os.path.normpath(file_path)
    parts = norm.split(os.sep)
    # tests/ directory, bench files
    if "tests" in parts or "benches" in parts:
        return True
    # Files named *_test.rs or test_*.rs
    basename = os.path.basename(norm)
    if basename.startswith("test_") or basename.endswith("_test.rs"):
        return True
    return False


def extract_content(tool_input):
    """Extract file_path and content from tool input."""
    file_path = tool_input.get("file_path", "")
    parts = []

    if "content" in tool_input:
        parts.append(tool_input["content"])
    if "new_string" in tool_input:
        parts.append(tool_input["new_string"])
    if "edits" in tool_input:
        for edit in tool_input["edits"]:
            if "new_string" in edit:
                parts.append(edit["new_string"])

    return file_path, "\n".join(parts)


def check_copyright_doc_comment(content, file_path):
    """
    Block copyright notices in //! (doc comments).
    Copyright must use // (regular comment), never //!.
    Reason: //! is public documentation — clippy analyzes it and flags CamelCase words.
    """
    violations = []
    for i, line in enumerate(content.split("\n"), 1):
        stripped = line.lstrip()
        if stripped.startswith("//!") and re.search(r"copyright|©|\(c\)", stripped, re.IGNORECASE):
            violations.append(f"  Line {i}: {stripped.strip()}")

    if violations:
        return (
            "\n=== CODE QUALITY: COPYRIGHT IN DOC COMMENT ===\n"
            f"File: {file_path}\n"
            "Copyright lines use //! (doc comment) — must use // instead.\n"
            "Reason: //! is public docs; clippy flags CamelCase words in it.\n"
            + "\n".join(violations)
            + "\n\nFix: change //! to // for copyright lines.\n"
            "================================================\n"
        )
    return None


def check_unwrap_in_production(content, file_path):
    """
    Block .unwrap() in production code (non-test files).
    Allows .unwrap() in test files, in #[cfg(test)] modules,
    and on lines with an explicit justification comment.
    """
    if is_test_file(file_path):
        return None

    # Only check .rs files
    if not file_path.endswith(".rs"):
        return None

    unwrap_pattern = re.compile(r"\.unwrap\(\)")
    violations = []
    lines = content.split("\n")
    in_test_module = False

    for i, line in enumerate(lines):
        stripped = line.lstrip()

        # Track #[cfg(test)] module boundaries
        if re.search(r'#\[cfg\(test\)\]', stripped):
            in_test_module = True
            continue

        if in_test_module:
            continue

        # Skip comments
        if stripped.startswith("//"):
            continue

        if unwrap_pattern.search(line):
            # Allow lines with explicit justification
            if "// SAFETY:" in line or "// OK:" in line:
                continue
            violations.append(f"  Line {i + 1}: {stripped.strip()}")

    if violations:
        return (
            "\n=== CODE QUALITY: .unwrap() IN PRODUCTION CODE ===\n"
            f"File: {file_path}\n"
            ".unwrap() found outside test code. Use .expect(\"reason\") or proper error handling.\n"
            + "\n".join(violations)
            + "\n\nFix: replace .unwrap() with .expect(\"descriptive message\") or ? operator.\n"
            "If intentional, add '// SAFETY: reason' or '// OK: reason' on the same line.\n"
            "====================================================\n"
        )
    return None


def main():
    try:
        hook_input = json.load(sys.stdin)
    except (json.JSONDecodeError, EOFError):
        sys.exit(0)

    tool_name = hook_input.get("tool_name", "")
    tool_input = hook_input.get("tool_input", {})

    if tool_name not in ("Write", "Edit", "MultiEdit"):
        sys.exit(0)

    file_path, content = extract_content(tool_input)

    if not file_path or not content.strip():
        sys.exit(0)

    if not is_under_repo(file_path):
        sys.exit(0)

    # Run all checks, collect violations
    errors = []

    msg = check_copyright_doc_comment(content, file_path)
    if msg:
        errors.append(msg)

    msg = check_unwrap_in_production(content, file_path)
    if msg:
        errors.append(msg)

    if errors:
        print("\n".join(errors), file=sys.stderr)
        sys.exit(2)

    sys.exit(0)


if __name__ == "__main__":
    main()
