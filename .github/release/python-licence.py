"""python-licence.py <wheel> <sdist> <name-version> <LICENSE>: fails unless both distributions of
the Python SDK carry exactly the given licence text, the wheel as <name-version>.dist-info/licenses/
LICENSE and the sdist as <name-version>/LICENSE."""

import sys
import tarfile
import zipfile

wheel, sdist, base, licence = sys.argv[1:]
want = open(licence, "rb").read()
try:
    with zipfile.ZipFile(wheel) as z:
        in_wheel = z.read(f"{base}.dist-info/licenses/LICENSE")
    with tarfile.open(sdist) as t:
        in_sdist = t.extractfile(f"{base}/LICENSE").read()
except (KeyError, AttributeError) as e:
    sys.exit(f"python-licence.py: a distribution has no licence file: {e}")
if in_wheel != want or in_sdist != want:
    sys.exit(f"python-licence.py: the licence in {wheel} or {sdist} isn't {licence}")
print(f"the MIT licence is in {wheel} and {sdist}")
