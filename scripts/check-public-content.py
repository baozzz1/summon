#!/usr/bin/env python3
"""扫描一棵源码树里不该公开的内容。

通用规则：凭据、像真的飞书 ID、本机用户路径、保留域名以外的邮箱、私有运行时文件、
符号链接和没审过的二进制。`--private-terms` 再叠一份只存在本机、不进仓库的名单
(真实的名字与 ID),由调用方生成。

命中只输出 `路径:行号: 类别`,从不打印命中的值，输出可以放心留在日志里。
"""
import argparse
import bisect
import hashlib
import itertools
import json
import math
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PRIVATE_SUFFIXES = {'.env', '.pem', '.key', '.p12', '.pfx', '.sqlite', '.sqlite3', '.db', '.log'}
PRIVATE_ROOTS = {'.summon', '.claude', 'state', 'runtime', 'sessions', 'audit', 'tasks'}

SECRET = re.compile(
    r'-----BEGIN (?:RSA |EC |DSA |OPENSSH |ENCRYPTED )?PRIVATE KEY-----'
    r'|\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{40,}|AKIA[A-Z0-9]{16}'
    r'|sk-(?:ant-|proj-)?[A-Za-z0-9_-]{32,}|xox[abpr]-[A-Za-z0-9-]{20,}|t-g[0-9a-f]{30,})\b'
)
# 以密钥命名的键后面跟着字面值。${VAR}、<VALUE>、{{var}}、空串这类占位不算。
SECRET_KEY = re.compile(
    r'''(?i)["']?\b(?:app_secret|client_secret|access_token|refresh_token|api_key|secret_key|password)\b["']?'''
    r'''\s*[:=]\s*["']([^"'\s]{12,})["']'''
)
PLACEHOLDER = re.compile(r'\$\{[A-Za-z_][A-Za-z0-9_]*\}|<[^<>]+>|\{\{[^{}]+\}\}')
FEISHU_ID = re.compile(r'\b(?:ou|on|oc|om|cli)_([0-9a-z]{16,})\b')
USER_PATH = re.compile(r'(?<![A-Za-z0-9_.-])/(?:Users|home)/([A-Za-z0-9][A-Za-z0-9_.-]*)(?=[/"\'\s`)]|$)')
GENERIC_USERS = {
    'me', 'you', 'user', 'username', 'example', 'alice', 'bob', 'carol', 'test', 'tester',
    'owner', 'nobody', 'someone', 'runner', 'ubuntu', 'admin', 'dev', 'demo', 'x', 'u',
    'foo', 'bar', 'linuxbrew', 'shared', 'name', 'xxx', 'bot', 'john', 'someone-else',
}
# URL 里 `user:pass@host` 的 userinfo、`pkg@1.2.3` 版本号、`user@1001.service` 这类单元名都不是邮箱。
EMAIL = re.compile(r'(?<![:A-Za-z0-9._%+-])[A-Za-z0-9._%+-]+@((?:[A-Za-z0-9-]+\.)*[A-Za-z0-9-]*[A-Za-z][A-Za-z0-9-]*\.[A-Za-z]{2,})\b')
RESERVED_DOMAIN_SUFFIXES = ('example.com', 'example.org', 'example.net', '.test', '.invalid',
                            '.example', 'localhost', 'users.noreply.github.com')
EMAIL_DOMAINS_OK = {'github.com', 'anthropic.com'}
HEX_RUN = re.compile(r'(?:\b(?:ou|on|oc|om|cli)_)?([0-9a-f]{8,})')
# 第三方文件原样随包，只查凭据和私有名单。
VENDORED_PREFIXES = ('crates/summon-core/assets/vendor/', 'crates/summon/assets/web_terminal/xterm.')
# 二进制文件要先审过内容与元数据才能公开；这里登记的摘要就是审过的记录。
REVIEWED_BINARIES = {
    'crates/summon/assets/livecard/breathe.gif':
        '66403fc5b1d0ceae65134ad78c69b466a6ca9cf435ffe6e83080b0e1afa97345',
    'crates/summon/assets/livecard/shimmer.gif':
        '48dc95ba49c04958bde1529e82be85cc1c5f470b9c42f6ab91e254d87c345921',
}


def synthetic_identifier(body):
    """一眼就是编出来的 ID(重复、全零段、周期串、成对字符、顺数字),而不是平台发的。"""
    if len(set(body)) <= 6 or '00000000' in body:
        return True
    for period in range(1, 17):
        if len(body) > period and all(body[i] == body[i % period] for i in range(len(body))):
            return True
    if len(body) % 2 == 0 and all(body[i] == body[i + 1] for i in range(0, len(body), 2)):
        return True
    digits = [int(ch) for ch in body if ch.isdigit()]
    return len(digits) >= 6 and all(b == a + 1 for a, b in zip(digits, digits[1:]))


def is_mailbox(domain):
    """单字母的二级域(`user@x.com`、`a@b.cn`)是示例写法，不当作真邮箱。"""
    domain = domain.lower()
    if domain in EMAIL_DOMAINS_OK or domain.endswith(RESERVED_DOMAIN_SUFFIXES):
        return False
    return len(domain.split('.')[-2]) > 1


def ascii_variants(term):
    """`night-owl` 也要认 `night_owl`、`night owl`、`nightowl`。"""
    parts = re.split(r'[-_ ]', term.lower())
    if len(parts) > 3:
        return {term.lower()}  # 长句式的群名只认原样，分隔符组合会指数膨胀
    return {''.join(itertools.chain.from_iterable(zip(parts, seps + ('',))))
            for seps in itertools.product('-_ ', repeat=len(parts) - 1)} | {''.join(parts)}


def is_word_char(ch):
    return ch.isascii() and ch.isalnum()


# 不长于这个长度的 ASCII 私有词只在词边界上算数(太短,当子串会撞上普通单词);更长的当子串找,
# 嵌在驼峰标识符、路径段里的也算。
SHORT_TERM = 4

# 含非 ASCII 字符的私有词(中文、中英混写的群名与人名)按折叠形态比对:文本与私有词都去掉行内空白、
# 转小写,「示例 Alpha 群」「示例alpha群」是同一个名字。换行不去掉:跨行拼出来的多半是两句不相干的话。
INLINE_SPACE = re.compile(r'[^\S\n]+')
# 折叠后不短于 FRAGMENT_TERM 个字符的私有词,另拦它的前缀与后缀(名字被截掉结尾或开头)。片段取折叠
# 长度的 FRAGMENT_RATIO、且不少于 FRAGMENT_MIN 个字符:8~10 个字符的名字截掉 2~4 个字符仍拦得下,
# 越长的名字容忍的截断越多。更短的片段容易撞上普通中文短语,宁可漏拦这类变体,也不让闸门天天误报。
# 只剩 ASCII 的片段不拦:它会撞上代码里的普通单词,纯 ASCII 的名字另有上面的变体与词边界规则。
# 两头同时截掉的变体不在此列。
FRAGMENT_TERM = 8
FRAGMENT_MIN = 6
FRAGMENT_RATIO = 0.6


def fold_term(value):
    return re.sub(r'\s+', '', value.lower())


def term_fragments(value):
    """一个私有词会被拦的前缀 / 后缀片段(折叠形态);不够长、或片段只剩 ASCII 时为空。

    闸门自测用它挑「截尾变体」的注入值,与扫描共用同一份阈值。
    """
    folded = fold_term(value)
    if len(folded) < FRAGMENT_TERM:
        return []
    size = max(FRAGMENT_MIN, math.ceil(len(folded) * FRAGMENT_RATIO))
    return [part for part in dict.fromkeys((folded[:size], folded[-size:])) if not part.isascii()]


def folded_needles(value):
    """一个含非 ASCII 字符的私有词要找的折叠串。

    够长的词只找前缀与后缀片段:整词出现时片段必然也出现,不必再找一遍整词。两个片段都只剩
    ASCII 时退回找整词。
    """
    folded = fold_term(value)
    if not folded:
        return set()
    return set(term_fragments(value)) or {folded}


class FoldedText:
    """去掉行内空白的小写文本;命中时再把折叠后的下标换回原文下标。"""

    def __init__(self, lower):
        self.lower = lower
        self.text = INLINE_SPACE.sub('', lower)
        self.runs = None

    def origin(self, index):
        # 下标换算表只在真有命中时才建:大多数文件一处都不命中,不为它们付这份开销。
        if self.runs is None:
            after, removed, total = [], [], 0
            for match in INLINE_SPACE.finditer(self.lower):
                total += match.end() - match.start()
                after.append(match.end() - total)  # 这段空白之后第一个字符的折叠下标
                removed.append(total)
            self.runs = (after, removed)
        after, removed = self.runs
        k = bisect.bisect_right(after, index)
        return index + (removed[k - 1] if k else 0)


class PrivateTerms:
    def __init__(self, path):
        data = json.loads(Path(path).read_text(encoding='utf-8'))
        self.ascii, self.other = [], {}
        for entry in data.get('terms', []):
            value, kind = entry['value'], 'private-' + entry['kind']
            if value.isascii():
                bounded = len(value) <= SHORT_TERM
                self.ascii.extend((variant, kind, bounded) for variant in ascii_variants(value))
            else:
                for needle in folded_needles(value):
                    self.other.setdefault(needle, (kind, frozenset(needle)))
        self.bodies = sorted({token.partition('_')[2] for token in data.get('ids', [])})
        self.heads = {body[:8] for body in self.bodies}

    def id_hit(self, body):
        # 截断的前缀和带尾巴的长串都算命中；至少比 8 位，太短的合成串会撞车。
        if body[:8] not in self.heads:
            return False
        return any(real.startswith(body) or body.startswith(real) for real in self.bodies)

    def findings(self, text, model_ids=True):
        """返回 (起, 止, 类别);偏移由调用方换成行号。`model_ids` 时放过 claude-<名> 形态的模型 id。"""
        hits = []
        lower = text.lower()
        for variant, kind, bounded in self.ascii:
            start = lower.find(variant)
            while start != -1:
                end = start + len(variant)
                on_boundary = ((start == 0 or not is_word_char(lower[start - 1]))
                               and (end == len(lower) or not is_word_char(lower[end])))
                # 模型 id 与模型名(claude-<名>)不是 agent 名。
                model = model_ids and lower[max(0, start - 7):start] in ('claude-', 'claude ')
                if (on_boundary or not bounded) and not model:
                    hits.append((start, end, kind))
                start = lower.find(variant, start + 1)
        if self.other:
            folded = FoldedText(lower)
            # 先按字符集筛:私有词里有一个字不在这份文本里,就不必在它上面 find 一遍。
            present = set(folded.text)
            for needle, (kind, chars) in self.other.items():
                if not chars <= present:
                    continue
                start = folded.text.find(needle)
                while start != -1:
                    end = start + len(needle)
                    hits.append((folded.origin(start), folded.origin(end - 1) + 1, kind))
                    start = folded.text.find(needle, start + 1)
        for match in HEX_RUN.finditer(lower):
            body = match.group(1)
            prefixed = match.group(0) != body
            if (prefixed or len(body) >= 12) and self.id_hit(body):
                hits.append((match.start(), match.end(), 'private-id'))
        return hits

    def redact(self, text):
        """把命中所在的整段路径换成 `***`:路径里带着私有词时,打印路径也不能把它带出去。

        所有路径命中(ASCII 别名、中文名、ID)都盖住它所在的整段,而不只盖命中的片段:截断变体
        命中的只是名字的前缀或后缀,ASCII 别名的变体也可能只命中一段文件名的一部分,只盖片段会把
        名字剩下的几个字留在输出里。代价是报出来的路径少了这一段的信息(例如
        `tests/<别名>.yaml` 报成 `tests/***`),靠其余路径段定位。
        """
        spans = []
        for start, end, _ in self.findings(text, model_ids=False):
            stop = text.find('/', end)
            spans.append((text.rfind('/', 0, start) + 1, len(text) if stop == -1 else stop))
        spans.sort()
        out, cursor = [], 0
        for start, end in spans:
            if start >= cursor:
                out.append(text[cursor:start])
                out.append('***')
                cursor = end
            elif end > cursor:
                cursor = end
        return ''.join(out) + text[cursor:]


def tree_entries(root):
    """要扫的文件。git 工作副本只扫入库的文件(就是会公开的那些,构建产物不算),给 CI 与本地用;
    别的目录整棵都扫,不跳过任何子目录 —— 导出闸门扫的是没有 .git 的导出目录,走的是这一支。"""
    if (root / '.git').exists():
        listed = subprocess.run(['git', '-C', str(root), 'ls-files', '-z'], capture_output=True,
                                check=True).stdout
        for name in sorted(os.fsdecode(p) for p in listed.split(b'\0') if p):
            path = root / name
            if path.is_symlink() or path.exists():
                yield path, Path(name)
        return
    for dirpath, dirnames, filenames in os.walk(root):
        links = [d for d in dirnames if os.path.islink(os.path.join(dirpath, d))]
        dirnames[:] = sorted(d for d in dirnames if d not in links)
        for name in sorted(filenames + links):
            path = Path(dirpath) / name
            yield path, path.relative_to(root)


def scan_file(root, path, rel, private):
    findings = set()
    rel_str = rel.as_posix()
    shown = rel_str
    if private is not None:
        # 路径本身也会公开:目录名、文件名里的私有词同样要拦,报出来的路径打码。
        # 路径里没有模型 id 要放行,claude-<名> 形态的目录名同样拦。
        path_hits = {kind for _, _, kind in private.findings(rel_str, model_ids=False)}
        if path_hits:
            shown = private.redact(rel_str)
            findings |= {(shown, 0, kind) for kind in path_hits}
    if (rel.parts[0] in PRIVATE_ROOTS or rel.name.startswith('.env')
            or rel.suffix.lower() in PRIVATE_SUFFIXES):
        findings.add((shown, 0, 'private-or-runtime-file'))
    if path.is_symlink():
        target = os.readlink(path)
        if private is not None:
            findings |= {(shown, 0, kind) for _, _, kind in private.findings(target, model_ids=False)}
        if os.path.isabs(target):
            findings.add((shown, 0, 'absolute-symlink'))
        else:
            resolved = (path.parent / target).resolve()
            if not resolved.is_relative_to(root.resolve()) or not resolved.is_file():
                findings.add((shown, 0, 'external-or-broken-symlink'))
        return findings
    data = path.read_bytes()
    if b'\0' in data[:8192] or rel.suffix.lower() in {'.gif', '.png', '.jpg', '.jpeg', '.webp', '.ico', '.pdf'}:
        if REVIEWED_BINARIES.get(rel_str) != hashlib.sha256(data).hexdigest():
            findings.add((shown, 0, 'binary-needs-review'))
        return findings
    text = data.decode('utf-8', errors='replace')
    if private is not None:
        line_starts = [0] + [m.end() for m in re.finditer('\n', text)]
        for offset, _, kind in private.findings(text):
            findings.add((shown, bisect.bisect_right(line_starts, offset), kind))
    vendored = rel_str.startswith(VENDORED_PREFIXES)
    for number, line in enumerate(text.splitlines(), 1):
        if SECRET.search(line):
            findings.add((shown, number, 'credential-pattern'))
        if vendored:
            continue
        if any(not PLACEHOLDER.fullmatch(m.group(1)) for m in SECRET_KEY.finditer(line)):
            findings.add((shown, number, 'literal-secret-value'))
        if any(not synthetic_identifier(m.group(1)) for m in FEISHU_ID.finditer(line)):
            findings.add((shown, number, 'realistic-feishu-id'))
        if any(m.group(1).lower() not in GENERIC_USERS for m in USER_PATH.finditer(line)):
            findings.add((shown, number, 'local-user-path'))
        if any(is_mailbox(m.group(1)) for m in EMAIL.finditer(line)):
            findings.add((shown, number, 'email-address'))
    return findings


def main(argv=None):
    parser = argparse.ArgumentParser(description='扫描一棵源码树里不该公开的内容。')
    parser.add_argument('--root', type=Path, default=ROOT, help='要扫的树(默认：本仓库)')
    parser.add_argument('--private-terms', type=Path, help='本机私有名单(JSON)')
    args = parser.parse_args(argv)
    private = PrivateTerms(args.private_terms) if args.private_terms else None
    findings = set()
    scanned = 0
    for path, rel in tree_entries(args.root):
        scanned += 1
        findings |= scan_file(args.root, path, rel, private)
    for rel, number, kind in sorted(findings):
        print(f'{rel}:{number}: {kind}')
    print(f'public-content: {scanned} 个文件，{len(findings)} 处命中', file=sys.stderr)
    return 1 if findings else 0


if __name__ == '__main__':
    sys.exit(main())
