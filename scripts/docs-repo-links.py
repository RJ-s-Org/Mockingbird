#!/usr/bin/env python3
"""mdBook preprocessor: rewrites relative links that leave the book.

A link in docs/ that resolves outside docs/ (../crates/mb-ext/README.md,
../../deploy/compose/README.md), or to a Markdown file that is not a chapter
(docs/superpowers, docs/launch), works on GitHub but not on the site. This
points it at the file on GitHub instead. Links between chapters, anchors and
absolute URLs are left alone.

Usage: listed in book.toml; mdBook runs it as `docs-repo-links.py supports
<renderer>` and then with `[context, book]` JSON on stdin.
"""
import json
import os
import re
import sys

REPO = "https://github.com/RJ-s-Org/Mockingbird"
LINK = re.compile(r"(\]\()([^)\s]+)(\))")
FENCE = re.compile(r"^\s*(```|~~~)")


def chapters(items):
    """Yields every chapter dict, depth first."""
    for item in items:
        chapter = item.get("Chapter") if isinstance(item, dict) else None
        if chapter:
            yield chapter
            yield from chapters(chapter.get("sub_items", []))


def rewrite(content, chapter_path, root, src, known):
    chapter_dir = os.path.dirname(chapter_path)

    def sub(match):
        target = match.group(2)
        if target.startswith(("#", "mailto:")) or "://" in target:
            return match.group(0)
        path, sep, fragment = target.partition("#")
        in_src = os.path.normpath(os.path.join(chapter_dir, path))
        repo_path = os.path.normpath(os.path.join(src, in_src))
        outside = in_src.startswith("..")
        stray_page = path.endswith(".md") and in_src not in known
        # A missing file stays as it is, so the link check reports it.
        if not (outside or stray_page) or not os.path.exists(os.path.join(root, repo_path)):
            return match.group(0)
        kind = "tree" if os.path.isdir(os.path.join(root, repo_path)) else "blob"
        url = f"{REPO}/{kind}/main/{repo_path}" + (sep + fragment if sep else "")
        return match.group(1) + url + match.group(3)

    out, fenced = [], False
    for line in content.split("\n"):
        if FENCE.match(line):
            fenced = not fenced
        out.append(line if fenced else LINK.sub(sub, line))
    return "\n".join(out)


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "supports":
        sys.exit(0)
    context, book = json.load(sys.stdin)
    root = context["root"]
    src = context["config"]["book"].get("src", "src")
    items = book.get("items", book.get("sections", []))
    found = list(chapters(items))
    known = {os.path.normpath(c["path"]) for c in found if c.get("path")}
    for chapter in found:
        if chapter.get("path"):
            chapter["content"] = rewrite(chapter["content"], chapter["path"], root, src, known)
    json.dump(book, sys.stdout)


if __name__ == "__main__":
    main()
