import subprocess
import os
import argparse
import glob
import requests
import tomllib
import re
import hashlib
import time

parser = argparse.ArgumentParser()
parser.add_argument("--upload", action="store_true")
parser.add_argument("--skip_cache", action="store_true")
parser.add_argument("--version")
parser.add_argument("--update_existing", action="store_true", help="Overwrite existing docker/v*.Dockerfile files.")
parser.add_argument(
    "--only_existing",
    action="store_true",
    help="When used with --update_existing, only update Dockerfiles that already exist locally.",
)
args = parser.parse_args()

# Array of Solana version mapped to rust version hashes
RUST_DOCKER_IMAGESHA_MAP = {
    "1.68.0": "sha256:79892de83d1af9109c47a4566a24a0b240348bb8c088f1bccc52645c4c70ec39"
}

RUST_VERSION_PLACEHOLDER = "$RUST_VERSION"
SOLANA_VERSION_PLACEHOLDER = "$SOLANA_VERSION"
AGAVE_VERSION_PLACEHOLDER = "$AGAVE_VERSION"
INSTALL_SHA256_PLACEHOLDER = "$INSTALL_SHA256"

_INSTALL_SHA256_CACHE = {}

def fetch_install_script_sha256(url: str) -> str:
    if url in _INSTALL_SHA256_CACHE:
        return _INSTALL_SHA256_CACHE[url]

    for _ in range(3):
        try:
            resp = requests.get(url, timeout=30)
            resp.raise_for_status()
            sha = hashlib.sha256(resp.content).hexdigest()
            _INSTALL_SHA256_CACHE[url] = sha
            return sha
        except Exception:
            time.sleep(2)

    _INSTALL_SHA256_CACHE[url] = None
    return None

def with_pinned_installer(url: str, install_path: str = "/tmp/solana_install.sh") -> str:
    return (
        f'RUN curl -sSfL "{url}" -o {install_path} && \\\n'
        f'    ACTUAL=$(sha256sum {install_path} | awk \'{{print $1}}\') && \\\n'
        f'    test "$ACTUAL" = "{INSTALL_SHA256_PLACEHOLDER}" && \\\n'
        f"    sh {install_path} && \\\n"
        f"    rm -f {install_path}\n"
    )

base_dockerfile_sol = f"""
FROM --platform=linux/amd64 rust@{RUST_VERSION_PLACEHOLDER}

RUN apt-get update && apt-get install -qy git gnutls-bin curl ca-certificates
{with_pinned_installer(f"https://release.solana.com/{SOLANA_VERSION_PLACEHOLDER}/install")}
ENV PATH="/root/.local/share/solana/install/active_release/bin:$PATH"
WORKDIR /build

CMD /bin/bash
"""

# Dockerfile template for Solana versions < 1.15
base_dockerfile_sol_pre15 = f"""
FROM --platform=linux/amd64 rust@{RUST_VERSION_PLACEHOLDER}

RUN apt-get update && apt-get install -qy git gnutls-bin curl ca-certificates

# Download and modify the Solana install script to install the specified version
RUN curl -sSfL "https://release.solana.com/v1.18.20/install" -o solana_install.sh && \\
    ACTUAL=$(sha256sum solana_install.sh | awk '{{print $1}}') && \\
    test "$ACTUAL" = "{INSTALL_SHA256_PLACEHOLDER}" && \\
    chmod +x solana_install.sh && \\
    sed -i "s/^SOLANA_INSTALL_INIT_ARGS=.*/SOLANA_INSTALL_INIT_ARGS={SOLANA_VERSION_PLACEHOLDER}/" solana_install.sh && \\
    ./solana_install.sh && \\
    rm solana_install.sh

ENV PATH="/root/.local/share/solana/install/active_release/bin:$PATH"
WORKDIR /build
CMD /bin/bash
"""

# Dockerfile template for Agave
base_dockerfile_agave = f"""
FROM --platform=linux/amd64 rust@{RUST_VERSION_PLACEHOLDER}

RUN apt-get update && apt-get install -qy git gnutls-bin curl ca-certificates
{with_pinned_installer(f"https://release.anza.xyz/{AGAVE_VERSION_PLACEHOLDER}/install")}
ENV PATH="/root/.local/share/solana/install/active_release/bin:$PATH"
# Call cargo test-sbf to trigger installation of associated platform tools
RUN cargo init --lib temp --edition 2021 && \\
    cd temp && \\
    echo "[lib]" >> Cargo.toml && \\
    echo 'crate-type = ["cdylib", "lib"]' >> Cargo.toml && \\
    cargo test-sbf && \\
    cd ../ && \\
    rm -rf temp
WORKDIR /build

CMD /bin/bash
"""

# Skipping these because the are not valid releases or where yanked or not released for mainnet
SKIPPED_VERSIONS = {
    "v1.15", "v1.14.0", "v1.10.0", "v1.10.16", "v1.10.18",
    "v1.10.27", "v1.10.36", "v1.10.37", "v1.11.7", "v1.11.8", "v1.11.9",
    "v1.13.0", 'v1.18.24', 'v2.0.12', 'v2.1.3', 'v2.1.12', "v2.1.19", "v2.2.25",
    "v2.2.5",
}

# Determine release information for Solana or Agave
def get_release_info(version_tag):
    """
    Determines if a version is a Solana or Agave release and provides relevant info.
    Returns a dictionary with base_dockerfile_text, version_placeholder, and the URL for the toolchain.
    """
    version_parts = version_tag.strip("v").split(".")
    if not all(part.isdigit() for part in version_parts):
        print(f"Skipping non-numeric tag: {version_tag}")
        return None

    major, minor, patch = map(int, version_parts)

    if major == 1 and minor == 15:
        print(f"Skipping yanked v1.15.x release: {version_tag}")
        return None

    if version_tag in SKIPPED_VERSIONS:
        print(f"Skipping yanked release: {version_tag}")
        return None

    # It would also be possible to generate more images for older versions but is it necessary?
    if major == 1 and minor < 10:
        print(f"Skipping all releases before 10 release: {version_tag}")
        return None
    
    # All releases 14 and below have a broken installer, so we need to use a custom script with a newer installer
    if (major == 1 and minor < 15):
        return {
            "base_dockerfile_text": base_dockerfile_sol_pre15,
            "version_placeholder": SOLANA_VERSION_PLACEHOLDER,
            "url": f"https://raw.githubusercontent.com/solana-labs/solana/{version_tag}/rust-toolchain.toml",
            "install_url": "https://release.solana.com/v1.18.20/install"
        }
    # Everything from 1.15.0 to 1.18.24 we want to get from solana labs 
    elif (major == 1 and minor >= 15) and not (minor == 18 and patch >= 24):
        return {
            "base_dockerfile_text": base_dockerfile_sol,
            "version_placeholder": SOLANA_VERSION_PLACEHOLDER,
            "url": f"https://raw.githubusercontent.com/solana-labs/solana/{version_tag}/rust-toolchain.toml",
            "install_url": f"https://release.solana.com/{version_tag}/install"
        }
    #Everything after we need to get from anza 
    elif (major == 1 and minor == 18 and patch >= 24) or major >= 2:
        return {
            "base_dockerfile_text": base_dockerfile_agave,
            "version_placeholder": AGAVE_VERSION_PLACEHOLDER,
            "url": f"https://raw.githubusercontent.com/anza-xyz/agave/{version_tag}/rust-toolchain.toml",
            "install_url": f"https://release.anza.xyz/{version_tag}/install"
        }

    return None

# Function to get Solana releases
def get_solana_releases():
    output = subprocess.check_output(
        ["git", "ls-remote", "--tags", "https://github.com/solana-labs/solana"]
    )
    tags = [
        elem.split("\t")[1].split("/")[-1]
        for elem in output.decode("utf-8").split("\n")
        if elem
    ]
    return tags

# Function to get Agave releases
def get_agave_releases():
    output = subprocess.check_output(
        ["git", "ls-remote", "--tags", "https://github.com/anza-xyz/agave"]
    )
    tags = [
        elem.split("\t")[1].split("/")[-1]
        for elem in output.decode("utf-8").split("\n")
        if elem
    ]
    return tags

def get_existing_docker_releases():
    """
    Returns release tags from existing dockerfiles only.
    Used to avoid network calls when we only need to update already-checked-in Dockerfiles.
    """
    releases = []
    for path in glob.glob("docker/v*.Dockerfile"):
        base = os.path.basename(path)
        if not base.endswith(".Dockerfile"):
            continue
        releases.append(base[: -len(".Dockerfile")])  # keep leading "v"
    return sorted(set(releases))

# Function to get Rust toolchain for each release
def get_toolchain(version_tag):
    if "v1.14" in version_tag:
        return "1.68.0"

    release_info = get_release_info(version_tag)
    if release_info is None:
        return None

    response = requests.get(release_info["url"], headers={"Accept": "application/vnd.github.v3.raw"})
    if response.status_code == 200:
        parsed_data = tomllib.loads(response.text)
        return parsed_data["toolchain"]["channel"]

    return get_rust_version_from_ci(version_tag)

def get_rust_version_from_ci(version_tag):
    url = f"https://raw.githubusercontent.com/solana-labs/solana/{version_tag}/ci/rust-version.sh"
    response = requests.get(url)
    if response.status_code == 200:
        match = re.search(r'stable_version=(\d+\.\d+\.\d+)', response.text)
        if match:
            return match.group(1)
        else:
            print(f"No stable Rust version found in {version_tag}")
    else:
        print(f"Failed to fetch rust-version.sh for {version_tag}")
    return None

# Process releases and generate Dockerfiles
def process_releases(releases):
    for release in releases:
        release_info = get_release_info(release)
        if release_info is None:
            continue

        path = f"docker/{release}.Dockerfile"
        if args.only_existing and not os.path.exists(path):
            continue

        base_dockerfile_text = release_info["base_dockerfile_text"]
        version_placeholder = release_info["version_placeholder"]

        rust_version = get_toolchain(release)
        print(f"Generating Dockerfile for {release} with Rust version {rust_version}")

        if rust_version is None:
            print(f"Skipping {release} due to missing Rust version.")
            continue

        if rust_version not in RUST_DOCKER_IMAGESHA_MAP and rust_version != "1.68.0":
            response = requests.get(
                f"https://hub.docker.com/v2/namespaces/library/repositories/rust/tags/{rust_version}"
            )
            if response.status_code == 200:
                for image in response.json()["images"]:
                    if image["architecture"] == "amd64":
                        RUST_DOCKER_IMAGESHA_MAP[rust_version] = image["digest"]
                        break
                if rust_version not in RUST_DOCKER_IMAGESHA_MAP:
                    print(f"Failed to fetch Rust image for {rust_version}")
                    continue

        dockerfile = base_dockerfile_text.replace(
            version_placeholder, release
        ).replace(
            RUST_VERSION_PLACEHOLDER, RUST_DOCKER_IMAGESHA_MAP[rust_version]
        ).lstrip("\n")

        if os.path.exists(path):
            if not args.update_existing:
                # Only generate Dockerfiles for new releases
                continue

        sha = fetch_install_script_sha256(release_info["install_url"])
        if not sha:
            print(f"Warning: failed to fetch installer checksum for {release}; leaving {path} unchanged")
            continue

        dockerfile = dockerfile.replace(INSTALL_SHA256_PLACEHOLDER, sha)

        if INSTALL_SHA256_PLACEHOLDER in dockerfile:
            raise Exception(f"Checksum not injected for {release}")

        stripped = release.strip("v")
        if os.path.exists(path):
            with open(path, "r") as f:
                unchanged = f.read() == dockerfile
        else:
            unchanged = False

        if not unchanged:
            dirty_set.add(stripped)
            with open(path, "w") as f:
                f.write(dockerfile)
        dockerfiles[release] = path

# Main execution
dockerfiles = {}
dirty_set = set()

if args.update_existing and args.only_existing:
    # Only update already present dockerfiles
    releases = get_existing_docker_releases()
    process_releases(releases)
else:
    solana_releases = get_solana_releases()
    agave_releases = get_agave_releases()
    process_releases(solana_releases)
    process_releases(agave_releases)

print(RUST_DOCKER_IMAGESHA_MAP)


digest_set = set()
if not args.skip_cache:
    print("Fetching existing images")
    response = requests.get(
        "https://hub.docker.com/v2/namespaces/solanafoundation/repositories/solana-verifiable-build/tags?page_size=1000"
    )
    for result in response.json()["results"]:
        print(result)
        if result["name"] != "latest":
            digest_set.add(result["name"])

if args.upload:
    print("Uploading all Dockerfiles")
    for tag, dockerfile in dockerfiles.items():
        # Strip the `v` from the tag to keep the versions consistent in Docker
        stripped_tag = tag.strip("v")

        (major, minor, patch) = stripped_tag.split(".")

        print(stripped_tag, args.version)

        force_build = False
        if args.version is not None:
            ver = args.version.split(".")
            if len(ver) == 2:
                a_major, a_minor = ver
                a_patch = patch
            else:
                a_major, a_minor, a_patch = ver
            if major != a_major or minor != a_minor or a_patch != patch:
                print(f"Skipping {stripped_tag}")
                continue
            force_build = True

        if (
            stripped_tag in digest_set
            and stripped_tag not in dirty_set
            and not force_build
        ):
            print(f"Already built image for {stripped_tag}, skipping")
            continue
        if stripped_tag in dirty_set:
            print(f"Dockerfile for {stripped_tag} needs to be modified")

        version_tag = f"solana:{stripped_tag}"
        print(version_tag)
        current_directory = os.getcwd()
        res = subprocess.call(
            f"docker build -t {version_tag} - < {current_directory}/{dockerfile}",
            shell=True,
        )
        if res == 0:
            subprocess.call(
                f"docker tag {version_tag} solanafoundation/solana-verifiable-build/{version_tag}", shell=True
            )
            subprocess.call(f"docker push solanafoundation/solana-verifiable-build/{version_tag}", shell=True)
        else:
            continue
