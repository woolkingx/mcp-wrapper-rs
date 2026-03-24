#!/usr/bin/env python3
"""Check for orphan MCP subprocess."""

import subprocess
import re

def main():
    print("=" * 60)
    print("MCP Orphan Check")
    print("=" * 60)

    # 1. Find all mcp-wrapper-rs PIDs
    result = subprocess.run(
        ['pgrep', '-f', '/home/claude/.cargo/bin/mcp-wrapper-rs'],
        capture_output=True, text=True
    )
    wrapper_pids = result.stdout.strip().split('\n') if result.stdout.strip() else []

    # 2. Check children under wrappers (via pstree)
    print(f"\n[1] Wrappers with children (pstree)")
    children_under_wrapper = 0
    child_pids = []

    for pid in wrapper_pids:
        if not pid:
            continue
        result = subprocess.run(['pstree', '-p', pid], capture_output=True, text=True)
        tree = result.stdout.strip()
        if tree and '---' in tree:
            children_under_wrapper += 1
            pids = re.findall(r'---\w+\((\d+)\)', tree)
            child_pids.extend(pids)
            print(f"  ! {tree}")

    if children_under_wrapper == 0:
        print("  ✓ None")

    # 3. Check orphans under init (PPID=1)
    print(f"\n[2] Orphans under init (PPID=1)")
    result = subprocess.run(
        ['ps', '-eo', 'pid,ppid,tty,etime,cmd'],
        capture_output=True, text=True
    )

    orphans = []
    for line in result.stdout.strip().split('\n')[1:]:
        parts = line.split(None, 4)
        if len(parts) < 5:
            continue
        pid, ppid, tty, etime, cmd = parts

        # PPID=1 and MCP-related
        if ppid == '1' and any(x in cmd for x in ['mcp-searxng', 'fetcher-mcp', 'ddg_search', 'sh -c "mcp', 'sh -c "npx']):
            orphans.append({'pid': pid, 'etime': etime, 'cmd': cmd[:60]})
            print(f"  ! [{pid}] ({etime}) {cmd[:60]}")

    if not orphans:
        print("  ✓ None")

    # Summary
    print("\n" + "=" * 60)
    total_issues = children_under_wrapper + len(orphans)
    if total_issues == 0:
        print("✓ All clean!")
    else:
        print(f"✗ {total_issues} issue(s) found")
        all_pids = child_pids + [o['pid'] for o in orphans]
        if all_pids:
            print(f"\nTo kill: kill {' '.join(all_pids)}")

    print(f"Wrappers: {len(wrapper_pids)}")
    print("=" * 60)

if __name__ == '__main__':
    main()
