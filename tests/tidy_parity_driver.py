"""Drives the Python `check` and `tidy` through the simulated hosts of tests/test_tidy.py and prints one transcript.
The Rust test (crates/gatetrain-core/tests/tidy_parity.rs) builds the same scenarios on its own simulation and compares the text.
Usage: python tests/tidy_parity_driver.py <scratch dir>   (the scratch dir holds the hosts files and the action logs)
"""
import contextlib, io, os, sys
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
sys.path.insert(0, str(HERE))
sys.stdout.reconfigure(newline="\n", encoding="utf-8")
import test_tidy as TT  # noqa: E402


def run(sim, d, name, extra, args, disk=True):
    d = Path(d)
    log = d / "l.jsonl"
    extra = dict(extra, log=str(log).replace("\\", "/"))
    conf = TT.write_conf(d, extra, disk)
    text = Path(conf).read_text()
    if sim.win:
        text = text.replace('os="linux"', 'os="windows"')
    Path(conf).unlink()
    (d / f"{name}.toml").write_text(text)
    conf = str(d / f"{name}.toml")
    out, err = io.StringIO(), io.StringIO()
    rc = None
    with mock.patch("gatetrain.tidy.dr.make_runner", return_value=sim), mock.patch("gatetrain.tidy.time.time", return_value=TT.NOW), \
            contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        from gatetrain.cli import main
        try:
            rc = main(list(args) + ["--config", conf])
        except SystemExit as e:
            rc = e.code if isinstance(e.code, int) else 1
            if isinstance(e.code, str):
                err.write(e.code + "\n")
    print(f"### {name} rc={rc}")
    sys.stdout.write(out.getvalue())
    print("--- stderr")
    sys.stdout.write(err.getvalue())
    if log.exists():
        print("--- log")
        sys.stdout.write(log.read_text())
        log.unlink()
    print("### end")


def main():
    d = sys.argv[1]
    for name, mk, extra, args in scenarios():
        run(mk(), d, name, extra, args)


def free(sim, gb):
    sim.free = gb
    return sim


def small_chain(sim):
    sim.free = 500
    sim.fs["/build/chain/debug/big.o"]["size"] = 1 * TT.MB
    return sim


def slow(path):
    def f():
        s = TT.linux_sim()
        s.slow = (path,)
        return s
    return f


def scenarios():
    L = TT.linux_sim
    return [
        ("check_text", L, {}, ["check"]),
        ("check_json", L, {}, ["check", "--json"]),
        ("check_ok", lambda: small_chain(L()), {}, ["check"]),
        ("check_slow_text", slow("/trash/old"), {}, ["check"]),
        ("check_slow_json", slow("/trash/old"), {}, ["check", "--json"]),
        ("check_all_slow", slow(""), {}, ["check"]),
        ("tidy_dry", L, {}, ["tidy"]),
        ("tidy_dry_class", L, {}, ["tidy", "--class", "trash,containers"]),
        ("tidy_auto", lambda: small_chain(L()), {}, ["tidy", "--auto"]),
        ("tidy_apply_all", L, {}, ["tidy", "--apply"]),
        ("tidy_apply_worktrees_win", TT_win, TT.WIN_EXTRA, ["tidy", "--apply", "--class", "worktrees"]),
        ("tidy_bad_class", L, {}, ["tidy", "--class", "bogus"]),
        ("tidy_unknown_host", L, {}, ["tidy", "--host", "nope"]),
    ]


def TT_win():
    return TT.AllWin.make()


class _W:
    pass


if __name__ == "__main__":
    # the win_sim helper lives inside ApplyTests in test_tidy.py
    TT.AllWin = _W()
    TT.AllWin.make = staticmethod(lambda: TT.ApplyTests("test_keep_list_path_is_never_removed")._win_sim())
    main()
