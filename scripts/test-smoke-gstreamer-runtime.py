#!/usr/bin/env python3
"""Fast smoke-driver regressions; no Docker daemon or runtime build required."""
import os
from pathlib import Path
import subprocess
import tempfile


root = Path(__file__).resolve().parent.parent
smoke = root / "scripts/smoke-gstreamer-runtime.sh"
with tempfile.TemporaryDirectory() as directory:
    work = Path(directory)
    docker = work / "docker"
    docker.write_text("""#!/usr/bin/env python3
import subprocess, sys
assert '-i' in sys.argv or '--interactive' in sys.argv, 'container stdin is closed'
script = sys.stdin.read()
assert 'gst-launch-1.0' in script, 'playback script was not delivered'
assert 'lumina-runtime-probe' in script, 'dynamic loader and TLS checks are missing'
subprocess.run(['bash', '-n'], input=script, text=True, check=True)
# A container failure must propagate through the driver.
sys.exit(42)
""")
    docker.chmod(0o755)
    artifact = work / "runtime.tar.gz"
    artifact.touch()
    fixture = work / "fixture.mp4"
    fixture.touch()
    result = subprocess.run(
        ["bash", str(smoke), str(artifact),
         str(root / "vendor/gstreamer-1.0.lock.json"), str(fixture)],
        env={**os.environ, "PATH": f"{work}:{os.environ['PATH']}"},
        capture_output=True, text=True,
    )
    assert result.returncode == 42, result.stderr
    assert "smoke passed" not in result.stdout

    # Exercise the production extraction pipeline against gst-inspect's format.
    parser = next(line.strip().removesuffix(")") for line in smoke.read_text().splitlines()
                  if "sed -n" in line and "Filename" in line)
    result = subprocess.run(
        ["bash", "-o", "pipefail", "-c", parser],
        input="Plugin Details:\n  Filename                 /runtime/libgstcoreelements.so\n",
        capture_output=True, text=True, check=True,
    )
    assert result.stdout.strip() == "/runtime/libgstcoreelements.so", result.stdout

print("smoke driver regressions passed")
