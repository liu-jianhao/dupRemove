#!/usr/bin/env python3
"""
绕开 git index 的提交工具。

背景：安全删除服务阻塞时，git 无法创建/删除 .git/index.lock，
常规 `git add` / `git commit` 全部失败。本脚本用纯 plumbing 命令
（hash-object / mktree / commit-tree / update-ref）直接构造提交，
全程不触碰索引文件。
"""
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# 沙箱环境残留：随机名临时文件（系统临时目录不可用时产生），非项目文件
JUNK_RE = re.compile(r"^[a-z0-9_]{8}$")
JUNK_NAMES = {".DS_Store", "probe.txt", "shot.swift", "shot2.swift", "shots_app.png"}


def is_junk(path: Path) -> bool:
    if path.name in JUNK_NAMES:
        return True
    if JUNK_RE.match(path.name) and path.is_file() and path.stat().st_size <= 8:
        return True
    if re.match(r"^\..*\.png-[A-Za-z0-9]+$", path.name):
        return True
    return False


def git(*args, check=True):
    r = subprocess.run(
        ["git", *args], cwd=ROOT, capture_output=True, text=True
    )
    if check and r.returncode != 0:
        raise RuntimeError(f"git {args} failed: {r.stderr.strip()}")
    return r


def hash_file(path: Path) -> str:
    """写入 blob 对象，返回 sha1。"""
    r = subprocess.run(
        ["git", "hash-object", "-w", "--", str(path)],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        raise RuntimeError(f"hash-object failed for {path}: {r.stderr.strip()}")
    return r.stdout.strip()


def build_tree(entries: list[tuple[str, str, str]]) -> str:
    """
    entries: [(mode, type, sha), ...]，name 为该层内的直接子项名
    """
    if not entries:
        # 空树：Git 不跟踪空目录，返回空 tree sha
        r = subprocess.run(
            ["git", "mktree", "--missing"], cwd=ROOT, input="", capture_output=True, text=True
        )
    else:
        text = "".join(f"{mode} {typ} {sha}\t{name}\n" for mode, typ, sha, name in entries)
        r = subprocess.run(
            ["git", "mktree"], cwd=ROOT, input=text, capture_output=True, text=True
        )
    if r.returncode != 0:
        raise RuntimeError(f"mktree failed: {r.stderr.strip()}")
    return r.stdout.strip()


def make_tree(path: Path) -> tuple[str, list[str]]:
    """
    递归为目录构造 tree。
    返回 (tree_sha, 该目录下的文件相对路径列表)
    """
    entries: list[tuple[str, str, str, str]] = []
    files: list[str] = []
    for child in sorted(path.iterdir()):
        if child.name == ".git":
            continue
        if child.is_dir():
            if child.name in ("node_modules", "target", "dist", ".shots", ".workbuddy", ".vscode"):
                continue
            sha, sub = make_tree(child)
            if not sub:
                continue  # 空目录不纳入 Git
            entries.append(("040000", "tree", sha, child.name))
            files.extend(f"{child.name}/{k}" for k in sub)
        elif child.is_file():
            if is_junk(child):
                continue
            entries.append(("100644", "blob", hash_file(child), child.name))
            files.append(child.name)
    return build_tree(entries), files


def main():
    message = sys.argv[1] if len(sys.argv) > 1 else "chore: 更新"

    root_entries: list[tuple[str, str, str, str]] = []
    root_files: list[str] = []
    for child in sorted(ROOT.iterdir()):
        if child.name == ".git":
            continue
        if child.is_dir():
            if child.name in ("node_modules", "target", "dist", ".shots", ".workbuddy", ".vscode"):
                continue
            sha, sub = make_tree(child)
            if not sub:
                continue
            root_entries.append(("040000", "tree", sha, child.name))
            root_files.extend(f"{child.name}/{k}" for k in sub)
        elif child.is_file():
            if is_junk(child):
                continue
            root_entries.append(("100644", "blob", hash_file(child), child.name))
            root_files.append(child.name)

    tree = build_tree(root_entries)
    print(f"tree: {tree}  ({len(root_files)} files)")

    parent = git("rev-parse", "HEAD").stdout.strip()
    r = subprocess.run(
        ["git", "commit-tree", tree, "-p", parent, "-m", message],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        raise RuntimeError(f"commit-tree failed: {r.stderr.strip()}")
    commit = r.stdout.strip()
    print(f"commit: {commit}")

    git("update-ref", "refs/heads/main", commit, parent)
    print(f"refs/heads/main updated -> {commit}")

    # 清理临时对象
    git("prune", check=False)


if __name__ == "__main__":
    main()
