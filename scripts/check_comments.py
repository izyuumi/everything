import argparse
from collections import Counter
from hashlib import sha256
from io import StringIO
import os
from pathlib import Path
import re
import subprocess
import tokenize
from typing import NamedTuple


LANGUAGES = {
    ".rs": "rust", ".swift": "swift", ".sh": "shell", ".py": "python",
    ".yaml": "yaml", ".yml": "yaml", ".toml": "toml", ".sql": "sql",
    ".js": "javascript", ".mjs": "javascript", ".ts": "javascript", ".html": "html",
}
EXCLUDED_SUFFIXES = {
    ".resolved", ".md", ".svg", ".json", ".plist", ".entitlements", ".xcscheme",
}
SHELL_SHEBANG = re.compile(r"#![ \t]*(?:\S*/)?(?:env[ \t]+(?:-S[ \t]+)?(?:\S*/)?)?(?:ba)?sh(?=\s|$)")
DIRECTIVE = re.compile(
    r"(?:swiftlint|swift-format|shellcheck|noqa|type: ?ignore|yaml-language-server|"
    r"eslint|prettier|SPDX-License-Identifier)\b"
)
TOKENS = {
    "rust": re.compile(r"""//|/\*|(?<!\w)[bc]?r\#*"|"|'(?:[^'\\\n]|\\(?:x[\da-fA-F]{2}|u\{[\da-fA-F_]+\}|[^\n]))'"""),
    "swift": re.compile(r'//|/\*|#*(?:"""|")|[()]'),
    "yaml": re.compile(r"""(?<!\S)#|(?<![^\s:[{,?])['"]"""),
    "toml": re.compile(r"""#|"{3}|'{3}|['"]"""),
    "sql": re.compile(r"""--|/\*|['"\x60\[]"""),
    "javascript": re.compile(r"""//|/\*|/|['"\x60{}]"""),
    "html": re.compile(r"""<!--|</?[a-zA-Z](?:"[^"]*"|'[^']*'|[^"'<>])*>"""),
    "gitignore": re.compile(r"^[^\S\n]*#", re.MULTILINE),
}
LINE_MARKERS = {"//", "#", "--"}
BLOCK_ENDS = {"/*": "*/", "<!--": "-->"}
C_BLOCK_TOKENS = re.compile(r"/\*|\*/")
JS_REGEX = re.compile(r"/(?:\\[^\n]|\[(?:\\[^\n]|[^\\\]\n])*\]|[^/\\[\n])+/[a-zA-Z]*")


class Comment(NamedTuple):
    line: int
    lang: str
    raw: str
    text: str
    allowed: bool


def language(path):
    if path.name == ".gitignore":
        return "gitignore"
    return LANGUAGES.get(path.suffix)


def line_end(source, start):
    end = source.find("\n", start)
    return len(source) if end == -1 else end


def quoted_end(source, pos, end, *, escape=False, doubled=False, extra_quotes=False):
    while pos < len(source):
        if escape and source[pos] == "\\":
            pos += 2
        elif source.startswith(end, pos):
            pos += len(end)
            if doubled and source.startswith(end, pos):
                pos += len(end)
                continue
            if extra_quotes:
                while pos < len(source) and source[pos] == end[0]:
                    pos += 1
            return pos
        else:
            pos += 1
    return len(source)


def block_end(source, pos, opening, closing, nested):
    if not nested:
        end = source.find(closing, pos)
        return len(source) if end == -1 else end + len(closing)
    depth = 1
    for match in C_BLOCK_TOKENS.finditer(source, pos):
        depth += 1 if match[0] == opening else -1
        if depth == 0:
            return match.end()
    return len(source)


def interpolated_end(source, lang, pos, end, escape, opening, brackets):
    while pos < len(source):
        if source.startswith(end, pos):
            return pos + len(end)
        if source.startswith(opening, pos):
            pos = yield from scan(source, lang, pos + len(opening), brackets)
        elif source.startswith(escape, pos):
            pos += len(escape) + 1
        else:
            pos += 1
    return len(source)


def regex_position(source, pos):
    while pos > 0 and source[pos - 1] in " \t\r":
        pos -= 1
    return pos == 0 or source[pos - 1] in "\n(,=:[!&|?{};"


def scan(source, lang, pos=0, brackets=None):
    if lang == "shell":
        return (yield from scan_shell(source, pos, brackets))
    depth = 1
    while match := TOKENS[lang].search(source, pos):
        token = match[0].lstrip()
        start, pos = match.end() - len(token), match.end()
        if token in LINE_MARKERS:
            pos = line_end(source, pos)
            yield start, pos, token, ""
        elif token in BLOCK_ENDS:
            closing = BLOCK_ENDS[token]
            pos = block_end(source, pos, token, closing, lang in {"rust", "swift"})
            yield start, pos, token, closing
        elif brackets and token in brackets:
            depth += 1 if token == brackets[0] else -1
            if depth == 0:
                return pos
        elif lang == "rust" and token.endswith('"'):
            pos = quoted_end(source, pos, '"' + "#" * token.count("#"), escape=token == '"')
        elif lang == "swift" and token not in "()":
            quotes = token.lstrip("#")
            hashes = token[:len(token) - len(quotes)]
            escape = "\\" + hashes
            pos = yield from interpolated_end(source, lang, pos, quotes + hashes, escape, escape + "(", "()")
        elif lang == "javascript":
            if token == "/" and regex_position(source, start):
                if regex := JS_REGEX.match(source, start):
                    pos = regex.end()
            elif token == "\x60":
                pos = yield from interpolated_end(source, lang, pos, token, "\\", "${", "{}")
            elif token in "'\"":
                pos = quoted_end(source, pos, token, escape=True)
        elif lang in {"yaml", "toml", "sql"}:
            end = "]" if token == "[" else token
            pos = quoted_end(
                source, pos, end,
                escape=lang != "sql" and token.startswith('"'),
                doubled=lang == "sql" and token != "[" or lang == "yaml" and token == "'",
                extra_quotes=lang == "toml" and len(token) == 3,
            )
    return len(source)


def scan_shell(source, pos, brackets):
    word_start, depth = True, 1
    while pos < len(source):
        char = source[pos]
        if char == "#" and word_start:
            end = line_end(source, pos)
            yield pos, end, "#", ""
            pos = end
        elif brackets and char in brackets:
            depth += 1 if char == brackets[0] else -1
            pos += 1
            if depth == 0:
                return pos
            word_start = True
        elif char == '"':
            pos = yield from interpolated_end(source, "shell", pos + 1, char, "\\", "$(", "()")
            word_start = False
        elif char == "'":
            pos = quoted_end(source, pos + 1, char)
            word_start = False
        elif char == "\\":
            if source[pos + 1:pos + 2] != "\n":
                word_start = False
            pos += 2
        else:
            word_start = char in " \t\n\r;|&()<>"
            pos += 1
    return len(source)


def make_comment(line, lang, raw, marker, closing="", *, first=False):
    body = raw[len(marker):].removesuffix(closing)
    if lang in {"rust", "swift"} and marker in {"//", "/*"}:
        doc = marker[-1]
        if (body.startswith(doc) and not body.startswith(doc * 2)) or (
            lang == "rust" and body.startswith("!")
        ):
            body = body[1:]
    text = body.strip()
    allowed = bool(DIRECTIVE.match(text) or first and raw.startswith("#!"))
    allowed |= lang == "rust" and marker == "//" and raw[2:].lstrip().startswith("SAFETY:")
    allowed |= lang == "python" and line <= 2 and bool(re.match(r"-\*-\s*coding\s*:", text))
    return Comment(line, lang, raw, text, allowed)


def lex_comments(path, source):
    path = Path(path)
    lang = language(path)
    if not lang and not path.suffix and SHELL_SHEBANG.match(source):
        lang = "shell"
    if lang == "python":
        for token in tokenize.generate_tokens(StringIO(source).readline):
            if token.type == tokenize.COMMENT:
                yield make_comment(token.start[0], lang, token.string, "#", first=token.start == (1, 0))
    elif lang:
        offset, line = 0, 1
        if lang in {"rust", "javascript"} and source.startswith("#!") and not source.startswith("#!["):
            offset = line_end(source, 0)
            yield make_comment(line, lang, source[:offset], "#", first=True)
        for start, end, marker, closing in scan(source, lang, offset):
            line += source.count("\n", offset, start)
            yield make_comment(line, lang, source[start:end], marker, closing, first=start == 0)
            offset = start


def tracked_comments(root):
    tracked = subprocess.check_output(["git", "ls-files", "-z"], cwd=root)
    for name in tracked.split(b"\0"):
        if not name:
            continue
        path = Path(os.fsdecode(name))
        if (
            path.suffix in EXCLUDED_SUFFIXES
            or path.name in {"Cargo.lock", "LICENSE"}
            or {"vendor", "third_party"}.intersection(path.parts[:-1])
            or path.parts[:3] == ("apps", "Apple", "Vorra.xcodeproj")
            or path.suffix and not language(path)
        ):
            continue
        file = root / path
        if file.is_symlink() or not file.is_file():
            continue
        if not language(path):
            with file.open("rb") as source:
                if not SHELL_SHEBANG.match(source.readline(4096).decode("utf-8", "replace")):
                    continue
        with (tokenize.open(file) if path.suffix == ".py" else file.open(encoding="utf-8")) as source:
            for comment in lex_comments(path, source.read()):
                yield path.as_posix(), comment


def report(path, comment, label, text):
    display = text.replace("\n", r"\n")
    print(f"{path}:{comment.line}: {label}: {display}")


def main():
    parser = argparse.ArgumentParser(description="Check the source comment ratchet")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--list", action="store_true", help="list all comments, including allowed ones")
    mode.add_argument("--update", action="store_true", help="remove stale baseline entries")
    args = parser.parse_args()
    root = Path(os.fsdecode(subprocess.check_output(["git", "rev-parse", "--show-toplevel"]).removesuffix(b"\n")))
    comments = tracked_comments(root)
    if args.list:
        for path, comment in comments:
            label = "allowed" if comment.allowed else "comment"
            report(path, comment, f"{label} ({comment.lang})", comment.raw)
        return 0

    baseline = root / "scripts/comment_baseline.txt"
    remaining = Counter(baseline.read_text(encoding="utf-8").splitlines())
    matched = []
    new = 0
    for path, comment in comments:
        if comment.allowed:
            continue
        digest = sha256(comment.text.encode("utf-8")).hexdigest()[:12]
        entry = f"{path}\t{digest}"
        if remaining[entry]:
            remaining[entry] -= 1
            matched.append(entry)
        else:
            new += 1
            report(path, comment, f"comment not allowed ({comment.lang})", comment.text)

    stale = sum(remaining.values())
    if args.update and new:
        print("Baseline update refused because it would add entries.")
    if stale and args.update and not new:
        baseline.write_text("".join(f"{entry}\n" for entry in sorted(matched)), encoding="utf-8")
        stale = 0
    if stale:
        print("Stale baseline entries. Run python3 scripts/check_comments.py --update.")
    print(f"comments: {len(matched)} baselined, {new} new, {stale} stale baseline entries")
    return int(bool(new or stale))


if __name__ == "__main__":
    raise SystemExit(main())
