import importlib
import importlib.metadata
import importlib.util
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

_SDK_PYTHON_ROOT = str(Path(__file__).resolve().parent)
if _SDK_PYTHON_ROOT not in sys.path:
    sys.path.insert(0, _SDK_PYTHON_ROOT)

from release_version import (  # noqa: E402
    normalize_codex_version as _normalized_package_version,
)

PACKAGE_NAME = "baicode-cli-bin"
SDK_PACKAGE_NAME = "baicode-sdk"


class RuntimeSetupError(RuntimeError):
    pass


def pinned_runtime_version() -> str:
    """Return the exact runtime version pinned by the SDK package dependency."""
    source_pin = _source_tree_runtime_dependency_version()
    if source_pin is not None:
        return _normalized_package_version(source_pin)

    try:
        installed_pin = _installed_sdk_runtime_dependency_version()
    except importlib.metadata.PackageNotFoundError as exc:
        raise RuntimeSetupError(
            f"Unable to resolve {SDK_PACKAGE_NAME} metadata for runtime pinning."
        ) from exc
    if installed_pin is None:
        raise RuntimeSetupError(
            f"Unable to resolve {PACKAGE_NAME} dependency pin from {SDK_PACKAGE_NAME}."
        )
    return _normalized_package_version(installed_pin)


def ensure_runtime_package_installed(
    python_executable: str | Path,
    sdk_python_dir: Path,
    install_target: Path | None = None,
) -> str:
    requested_version = pinned_runtime_version()
    installed_version = None
    if install_target is None:
        installed_version = _installed_runtime_version(python_executable)
    normalized_requested = _normalized_package_version(requested_version)

    if (
        installed_version is not None
        and _normalized_package_version(installed_version) == normalized_requested
    ):
        return requested_version

    package = os.environ.get("BAICODE_RUNTIME_PACKAGE")
    if not package or not Path(package).exists():
        raise RuntimeSetupError(
            f"Install {PACKAGE_NAME}=={requested_version} or set BAICODE_RUNTIME_PACKAGE "
            "to a local BaiCode package directory or .tar.gz archive."
        )

    with tempfile.TemporaryDirectory(prefix="baicode-python-runtime-") as temp_root_str:
        temp_root = Path(temp_root_str)
        staged_runtime_dir = _stage_runtime_package(
            sdk_python_dir,
            requested_version,
            Path(package),
            temp_root / "runtime-stage",
        )
        _install_runtime_package(python_executable, staged_runtime_dir, install_target)

    if install_target is not None:
        return requested_version

    if Path(python_executable).resolve() == Path(sys.executable).resolve():
        importlib.invalidate_caches()

    installed_version = _installed_runtime_version(python_executable)
    if (
        installed_version is None
        or _normalized_package_version(installed_version) != normalized_requested
    ):
        raise RuntimeSetupError(
            f"Expected {PACKAGE_NAME} {requested_version} in {python_executable}, "
            f"but found {installed_version!r} after installation."
        )
    return requested_version


def _installed_runtime_version(python_executable: str | Path) -> str | None:
    snippet = (
        "import importlib.metadata, json, sys\n"
        "try:\n"
        "    from baicode_cli_bin import bundled_codex_path\n"
        "    bundled_codex_path()\n"
        f"    print(json.dumps({{'version': importlib.metadata.version({PACKAGE_NAME!r})}}))\n"
        "except Exception:\n"
        "    sys.exit(1)\n"
    )
    result = subprocess.run(
        [str(python_executable), "-c", snippet],
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        return None
    return json.loads(result.stdout)["version"]


def _stage_runtime_package(
    sdk_python_dir: Path,
    runtime_version: str,
    runtime_package_archive: Path,
    staging_dir: Path,
) -> Path:
    script_module = _load_update_script_module(sdk_python_dir)
    return script_module.stage_python_runtime_package(  # type: ignore[no-any-return]
        staging_dir,
        runtime_version,
        runtime_package_archive.resolve(),
    )


def _install_runtime_package(
    python_executable: str | Path,
    staged_runtime_dir: Path,
    install_target: Path | None,
) -> None:
    args = [
        str(python_executable),
        "-m",
        "pip",
        "install",
        "--no-index",
        "--no-build-isolation",
        "--force-reinstall",
        "--no-deps",
    ]
    if install_target is not None:
        install_target.mkdir(parents=True, exist_ok=True)
        args.extend(["--target", str(install_target)])
    args.append(str(staged_runtime_dir))
    try:
        subprocess.run(
            args,
            check=True,
            text=True,
            capture_output=True,
        )
    except subprocess.CalledProcessError as exc:
        raise RuntimeSetupError(
            f"Failed to install {PACKAGE_NAME} into {python_executable} from {staged_runtime_dir}.\n"
            f"STDOUT:\n{exc.stdout}\nSTDERR:\n{exc.stderr}"
        ) from exc


def _load_update_script_module(sdk_python_dir: Path):
    script_path = sdk_python_dir / "scripts" / "update_sdk_artifacts.py"
    spec = importlib.util.spec_from_file_location("update_sdk_artifacts", script_path)
    if spec is None or spec.loader is None:
        raise RuntimeSetupError(f"Failed to load {script_path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def _source_tree_runtime_dependency_version() -> str | None:
    """Read the runtime dependency pin when the SDK is running from a checkout."""
    pyproject_path = Path(__file__).resolve().parent / "pyproject.toml"
    if not pyproject_path.exists():
        return None

    match = re.search(_runtime_dependency_pin_pattern(), pyproject_path.read_text())
    if match is None:
        return None
    return match.group(1)


def _installed_sdk_runtime_dependency_version() -> str | None:
    """Read the runtime dependency pin from installed package metadata."""
    requirements = importlib.metadata.requires(SDK_PACKAGE_NAME) or []
    for requirement in requirements:
        match = re.search(_runtime_dependency_pin_pattern(), requirement)
        if match is not None:
            return match.group(1)
    return None


def _runtime_dependency_pin_pattern() -> str:
    """Match the exact runtime dependency pin in TOML and wheel metadata."""
    return rf'{re.escape(PACKAGE_NAME)}\s*==\s*"?([^",;\s]+)"?'


__all__ = [
    "PACKAGE_NAME",
    "SDK_PACKAGE_NAME",
    "RuntimeSetupError",
    "ensure_runtime_package_installed",
    "pinned_runtime_version",
]
