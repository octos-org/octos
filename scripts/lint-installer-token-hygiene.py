#!/usr/bin/env python3
"""Lint installer secret hygiene for the serve bearer token.

#2371 moved the token off argv; #2388 closes the remaining local-read
exposure: the installers still shipped it in world-readable files — the
0644 systemd unit / launchd plist, and the Windows serve-launcher.cmd
wrapper. This lint pins the resulting invariants so they don't rot:

1. install.sh — the generated systemd unit carries no secrets inline and
   loads `{DATA_DIR}/serve.env` via EnvironmentFile; the launchd plist is
   installed 0600 (launchd reads it as root; no EnvironmentFile exists).
2. install.ps1 — the serve-launcher.cmd wrapper carries no inline token;
   it reads a sibling `serve-token` file whose ACLs are restricted.
3. Render check — install.sh's service-writer functions are executed for
   real with a stubbed `sudo` (system paths rewritten into a sandbox),
   and the produced unit / plist / serve.env are asserted on both
   content and permission bits.

Scope: this covers `scripts/install.sh` and `scripts/install.ps1` — the
two surfaces #2388 named. Sibling install paths with the same threat
model are deliberately out of scope here (they need their own review):
`scripts/local-tenant-deploy.sh`, `scripts/frp/bootstrap-tenant.sh`
(both inline the token into their unit/plist templates) and
`scripts/deploy.ps1` (NSSM AppEnvironmentExtra, whose registry value is
readable by local users).

Known blind spots (deliberate): the manual-run `hint` lines echo the
token to the terminal (not a file), and install.ps1's doctor/uninstall
modes are only checked textually — they never write the token.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tempfile
from pathlib import Path

INSTALL_SH = Path("scripts/install.sh")
INSTALL_PS1 = Path("scripts/install.ps1")

# Functions needed to execute write_octos_service in isolation. If an
# entry rots out of install.sh the harness fails with command-not-found,
# so this list cannot silently over- or under-match.
SERVICE_WRITER_FUNCTIONS = [
    "section",
    "ok",
    "warn",
    "hint",
    "xml_escape",
    "launchd_env_var_xml",
    "systemd_env_var_line",
    "write_smtp_secret_file",
    "write_serve_env_file",
    "write_octos_service",
]

FAKE_TOKEN = "e2b07f0b1a6c4de0afc51dc39f74b2218ad6cc9f5b034e72b8a1d904c5ef6317"


def extract_function(text: str, name: str) -> str:
    """Return the top-level `name() { ... }` definition from install.sh text."""
    lines = text.splitlines()
    start = None
    for i, line in enumerate(lines):
        if re.match(rf"^{re.escape(name)}\(\)", line):
            start = i
            break
    if start is None:
        raise AssertionError(f"function {name}() not found in install.sh")
    out = [lines[start]]
    if re.search(r";\s*}\s*$", lines[start]):  # single-line definition
        return "\n".join(out)
    for line in lines[start + 1 :]:
        out.append(line)
        if line == "}":
            return "\n".join(out)
    raise AssertionError(f"function {name}() has no closing brace")


def extract_service_functions(text: str) -> str:
    # Tolerant on purpose: a function missing from install.sh (say, a
    # rename) fails loudly at its call site inside the harness instead.
    parts = []
    for name in SERVICE_WRITER_FUNCTIONS:
        try:
            parts.append(extract_function(text, name))
        except AssertionError:
            pass
    return "\n\n".join(parts)


def check_install_sh(text: str) -> list[str]:
    try:
        writer = extract_function(text, "write_octos_service")
    except AssertionError as exc:
        return [str(exc)]
    problems: list[str] = []

    if "Environment=OCTOS_AUTH_TOKEN=" in writer:
        problems.append(
            "systemd unit template inlines OCTOS_AUTH_TOKEN — the unit is "
            "installed world-readable (0644); secrets must load from "
            "{DATA_DIR}/serve.env via EnvironmentFile (#2388)"
        )
    if 'systemd_env_var_line "FRPS_TOKEN"' in writer:
        problems.append(
            'systemd unit template carries FRPS_TOKEN via systemd_env_var_line — '
            "it must live in the 0600 serve.env like the auth token (#2388)"
        )
    if "EnvironmentFile=$DATA_DIR/serve.env" not in writer:
        problems.append(
            "systemd unit template does not load EnvironmentFile=$DATA_DIR/serve.env"
        )
    if "write_serve_env_file" not in writer:
        problems.append("write_octos_service never writes serve.env (Linux branch)")
    if re.search(r"chmod 644 \"\$plist\"", writer):
        problems.append(
            "launchd serve plist is installed 0644 — it carries the token, "
            "install it 0600 root:wheel (#2388)"
        )
    if 'chmod 600 "$plist"' not in writer:
        problems.append("launchd serve plist is not installed 0600")

    try:
        env_writer = extract_function(text, "write_serve_env_file")
    except AssertionError:
        problems.append(
            "install.sh lacks write_serve_env_file — the 0600 serve.env "
            "writer the systemd unit loads (#2388)"
        )
        return problems
    if 'chmod 600 "$target"' not in env_writer:
        problems.append("write_serve_env_file does not chmod 600 the env file")
    if "OCTOS_AUTH_TOKEN=" not in env_writer:
        problems.append("write_serve_env_file does not write OCTOS_AUTH_TOKEN")
    return problems


def check_install_ps1(text: str) -> list[str]:
    problems: list[str] = []
    if 'set "OCTOS_AUTH_TOKEN=' in text:
        problems.append(
            "install.ps1 embeds the token inline in serve-launcher.cmd — the "
            "wrapper is world-readable on default ACLs; read the restricted "
            "serve-token file instead (#2388)"
        )
    if "set /p OCTOS_AUTH_TOKEN=<" not in text:
        problems.append(
            "serve-launcher.cmd does not read OCTOS_AUTH_TOKEN from the "
            "restricted serve-token file"
        )
    if "icacls $tokenTmp /inheritance:r" not in text:
        problems.append(
            "serve-token file is not ACL-restricted (icacls /inheritance:r)"
        )
    return problems


def render_install_sh(source: str, os_name: str, frps_token: str, sandbox: Path) -> dict[str, str]:
    """Execute install.sh's write_octos_service with a sandboxed sudo.

    Returns {relative path -> file contents} for every artifact the real
    function produced. `sudo` rewrites its trailing /Library, /etc and
    /usr target into the sandbox so mv/chmod actually run; chown and the
    service managers are no-ops.
    """
    data_dir = sandbox / "data"
    fake_root = sandbox / "root"
    tmpdir = sandbox / "tmp"
    for sub in (
        data_dir,
        fake_root / "Library" / "LaunchDaemons",
        fake_root / "etc" / "systemd" / "system",
        tmpdir,
    ):
        sub.mkdir(parents=True, exist_ok=True)

    functions = extract_service_functions(source)
    harness = f"""
set -euo pipefail
export TMPDIR='{tmpdir}'
export HOME='{sandbox / "home"}'
DATA_DIR='{data_dir}'
CONFIG_HOME='{data_dir / "config"}'
XDG_CONFIG_HOME=''
PREFIX='{sandbox / "prefix"}'
OCTOS_BIN='{sandbox / "prefix" / "octos"}'
PORT='8080'
AUTH_TOKEN='{FAKE_TOKEN}'
FRPS_TOKEN='{frps_token}'
SMTP_PASSWORD=''
SMTP_HOST=''
SMTP_PORT=''
SMTP_USERNAME=''
SMTP_FROM=''
SUDO_USER='operator'
OS='{os_name}'

FAKE_ROOT='{fake_root}'
sudo() {{
    local args=("$@")
    local last=$(( ${{#args[@]}} - 1 ))
    case "${{args[$last]}}" in
        /Library/*|/etc/*|/usr/*) args[$last]="$FAKE_ROOT${{args[$last]}}" ;;
    esac
    case "${{args[0]}}" in
        chown|launchctl|systemctl) return 0 ;;
    esac
    "${{args[@]}}"
}}
launchctl() {{ return 0; }}
systemctl() {{ return 0; }}

{functions}

write_octos_service
"""
    subprocess.run(
        ["bash", "-c", harness],
        check=True,
        capture_output=True,
        text=True,
        timeout=60,
    )

    artifacts: dict[str, str] = {}
    candidates = [
        fake_root / "Library" / "LaunchDaemons" / "io.octos.serve.plist",
        fake_root / "etc" / "systemd" / "system" / "octos-serve.service",
        data_dir / "serve.env",
        data_dir / "smtp_secret.json",
    ]
    for path in candidates:
        if path.exists():
            artifacts[str(path.relative_to(sandbox))] = path.read_text()
    return artifacts


def mode_of(path: Path) -> int:
    out = subprocess.run(
        ["python3", "-c", "import os,sys; print(oct(os.stat(sys.argv[1]).st_mode & 0o7777))", str(path)],
        check=True,
        capture_output=True,
        text=True,
    )
    return int(out.stdout.strip(), 8)


def check_rendered(os_name: str, frps_token: str, artifacts: dict[str, str], sandbox: Path) -> list[str]:
    problems: list[str] = []
    tag = f"render[{os_name}]"

    if os_name == "Darwin":
        plist_rel = "root/Library/LaunchDaemons/io.octos.serve.plist"
        plist = sandbox / plist_rel
        if not plist.exists():
            return [f"{tag}: launchd plist was not produced"]
        if mode_of(plist) != 0o600:
            problems.append(f"{tag}: plist mode is {oct(mode_of(plist))}, want 600")
        # launchd has no EnvironmentFile: the plist is the only token
        # channel on macOS, protected by the 0600 mode asserted above.
        if FAKE_TOKEN not in artifacts.get(plist_rel, ""):
            problems.append(f"{tag}: plist lost the token — macOS serves would silently re-key")
    else:
        unit = sandbox / "root/etc/systemd/system/octos-serve.service"
        if not unit.exists():
            return [f"{tag}: systemd unit was not produced"]
        unit_text = artifacts.get("root/etc/systemd/system/octos-serve.service", "")
        if FAKE_TOKEN in unit_text or "OCTOS_AUTH_TOKEN" in unit_text:
            problems.append(f"{tag}: systemd unit carries the token inline")
        if "FRPS_TOKEN" in unit_text:
            problems.append(f"{tag}: systemd unit carries FRPS_TOKEN inline")
        if "EnvironmentFile=" not in unit_text:
            problems.append(f"{tag}: systemd unit lacks EnvironmentFile=")
        if mode_of(unit) != 0o644:
            problems.append(f"{tag}: unit mode is {oct(mode_of(unit))}, want 644")

        serve_env = sandbox / "data" / "serve.env"
        if not serve_env.exists():
            problems.append(f"{tag}: serve.env was not written")
        else:
            if mode_of(serve_env) != 0o600:
                problems.append(f"{tag}: serve.env mode is {oct(mode_of(serve_env))}, want 600")
            env_text = serve_env.read_text()
            if FAKE_TOKEN not in env_text:
                problems.append(f"{tag}: serve.env does not carry the auth token")
            frps_lines = [line for line in env_text.splitlines() if line.startswith("FRPS_TOKEN=")]
            if frps_token and not frps_lines:
                problems.append(f"{tag}: serve.env dropped a non-empty FRPS_TOKEN")
            if not frps_token and frps_lines:
                problems.append(f"{tag}: serve.env wrote an empty FRPS_TOKEN")
    return problems


def run_checks(repo: Path) -> list[str]:
    problems: list[str] = []
    install_sh = repo / INSTALL_SH
    install_ps1 = repo / INSTALL_PS1

    problems += [f"{INSTALL_SH}: {p}" for p in check_install_sh(install_sh.read_text())]
    problems += [f"{INSTALL_PS1}: {p}" for p in check_install_ps1(install_ps1.read_text())]

    with tempfile.TemporaryDirectory(prefix="token-hygiene-") as tmp:
        sandbox = Path(tmp)
        for os_name in ("Darwin", "Linux"):
            for frps_token in ("", "frps-shared-secret"):
                try:
                    artifacts = render_install_sh(install_sh.read_text(), os_name, frps_token, sandbox)
                except subprocess.CalledProcessError as exc:
                    problems.append(f"render[{os_name},frps={bool(frps_token)}] failed:\n{exc.stderr}")
                    continue
                problems += check_rendered(os_name, frps_token, artifacts, sandbox)
    return problems


def self_test() -> int:
    failures: list[str] = []

    def expect_problems(where: str, got: list[str], wanted_substring: str) -> None:
        if not got:
            failures.append(f"{where}: expected a problem mentioning {wanted_substring!r}, got none")
        elif not any(wanted_substring in p for p in got):
            failures.append(f"{where}: expected a problem mentioning {wanted_substring!r}, got {got}")

    bad_unit = (
        "write_octos_service() {\n"
        "    cat > \"$tmp\" << EOF\n"
        "Environment=OCTOS_AUTH_TOKEN=$AUTH_TOKEN\n"
        "EOF\n"
        "            sudo chmod 644 \"$unit\"\n"
        "            sudo chmod 644 \"$plist\"\n"
        "}\n"
    )
    expect_problems("bad unit", check_install_sh(bad_unit), "inlines OCTOS_AUTH_TOKEN")
    expect_problems("bad unit", check_install_sh(bad_unit), "0644")

    good_unit = (
        "write_octos_service() {\n"
        "            write_serve_env_file\n"
        "    cat > \"$tmp\" << EOF\n"
        "EnvironmentFile=$DATA_DIR/serve.env\n"
        "EOF\n"
        "            sudo chmod 600 \"$plist\"\n"
        "}\n"
        "write_serve_env_file() {\n"
        "    printf 'OCTOS_AUTH_TOKEN=\"%s\"\\n' \"$AUTH_TOKEN\" > \"$target\"\n"
        "    chmod 600 \"$target\"\n"
        "}\n"
    )
    if check_install_sh(good_unit):
        failures.append(f"good unit flagged: {check_install_sh(good_unit)}")

    bad_wrapper = '@echo off\nset "OCTOS_AUTH_TOKEN=secret"\n'
    expect_problems("bad wrapper", check_install_ps1(bad_wrapper), "embeds the token inline")
    good_wrapper = (
        "@echo off\n"
        'set /p OCTOS_AUTH_TOKEN=<"C:\\dir\\serve-token"\n'
        "icacls $tokenTmp /inheritance:r /grant:r \"${env:USERNAME}:F\"\n"
    )
    if check_install_ps1(good_wrapper):
        failures.append(f"good wrapper flagged: {check_install_ps1(good_wrapper)}")

    # The extractor must reject junk instead of silently producing nothing.
    try:
        extract_function("write_octos_service() {\nnever closed\n", "write_octos_service")
        failures.append("extractor accepted an unterminated function")
    except AssertionError:
        pass

    if failures:
        for f in failures:
            print(f"FAIL: {f}")
        return 1
    print("ok: self-test")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="run the checker against synthetic fixtures instead of the repo",
    )
    args = parser.parse_args()
    if args.self_test:
        return self_test()

    repo = Path(__file__).resolve().parent.parent
    problems = run_checks(repo)
    if problems:
        for p in problems:
            print(f"FAIL: {p}")
        return 1
    print("ok: installer token hygiene (#2388)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
