"""ci-steps.py OUT JOB...: runs the `run:` steps of CI's jobs from .github/workflows/ci.yml as
they stand, each as GitHub runs one (bash --noprofile --norc -eo pipefail), with the
workflow's, the job's and the step's env and its working-directory. Actions are not run;
setup() below stands in for the toolchain ones and for checkout's submodules. Output:
OUT/<job>-<n>-<name>.log whole, OUT/status one line per step. A job stops at its first
failing step."""
import os
import re
import subprocess
import sys
import time

import yaml

out, jobs = sys.argv[1], sys.argv[2:]
os.makedirs(out, exist_ok=True)
root = os.getcwd()
wf = yaml.safe_load(open(".github/workflows/ci.yml"))
head = subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
temp = "/tmp/runner-temp"
os.makedirs(temp, exist_ok=True)
status = open(os.path.join(out, "status"), "a", buffering=1)
status.write(f"head {head}\n")


def expand(v, env):
    def one(m):
        e = m.group(1).strip()
        if e == "github.sha":
            return head
        if e == "github.workspace":
            return root
        if e == "runner.temp":
            return temp
        if e == "runner.os":
            return "Linux"
        if e.startswith("env."):
            return env[e[4:]]
        raise SystemExit(f"unsupported expression {e!r}")
    return re.sub(r"\$\{\{(.*?)\}\}", one, str(v))


def run(name, script, env, cwd):
    log = os.path.join(out, name + ".log")
    t = time.time()
    with open(log, "w") as f:
        rc = subprocess.run(["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", script],
                            env=env, cwd=cwd, stdout=f, stderr=subprocess.STDOUT).returncode
    return rc, time.time() - t


def setup(job, step, env):
    """The toolchain actions and checkout's submodules, as shell."""
    uses, w = step["uses"], step.get("with", {})
    if uses.startswith("actions/checkout"):
        if w.get("submodules"):
            return "git submodule sync --recursive && git submodule update --init --force --depth=1"
        return None
    if uses.startswith("dtolnay/rust-toolchain"):
        tc = expand(w["toolchain"], env)
        args = ["rustup", "toolchain", "install", tc, "--profile", "minimal"]
        for c in str(w.get("components", "")).replace(",", " ").split():
            args += ["-c", c]
        for t in str(w.get("targets", "")).replace(",", " ").split():
            args += ["-t", t]
        env["RUSTUP_TOOLCHAIN"] = tc
        return " ".join(args)
    if uses.startswith("oven-sh/setup-bun"):
        return f"command -v bun || curl -fsSL https://bun.sh/install | bash -s bun-v{w['bun-version']}"
    return None


base = dict(os.environ)
base.update({k: expand(v, base) for k, v in wf.get("env", {}).items()})
for job in jobs:
    spec = wf["jobs"][job]
    env = dict(base)
    env.update({k: expand(v, env) for k, v in spec.get("env", {}).items()})
    env["RUNNER_TEMP"] = temp
    wd = spec.get("defaults", {}).get("run", {}).get("working-directory")
    for n, step in enumerate(spec["steps"], 1):
        name = f"{job}-{n:02d}-" + re.sub(r"[^a-z0-9]+", "-", step.get("name", step.get("uses", "run")).lower()).strip("-")
        if "if" in step:
            status.write(f"{job} {n:02d} has if: {step['if']}: run anyway\n")
        if "uses" in step:
            script = setup(job, step, env)
            if script is None:
                status.write(f"{job} {n:02d} {step['uses']}: action not run here\n")
                continue
            rc, secs = run(name, script, env, root)
            status.write(f"{job} {n:02d} {step['uses']} (stand-in)-exit {rc} {secs:.0f}s\n")
            if step["uses"].startswith("oven-sh/setup-bun"):
                env["PATH"] = os.path.expanduser("~/.bun/bin") + ":" + env["PATH"]
            continue
        senv = dict(env)
        senv.update({k: expand(v, env) for k, v in step.get("env", {}).items()})
        cwd = os.path.join(root, step.get("working-directory", wd or "."))
        rc, secs = run(name, step["run"], senv, cwd)
        status.write(f"{name}-exit {rc} {secs:.0f}s\n")
        if rc != 0:
            status.write(f"{job} stopped at {name}\n")
            break
