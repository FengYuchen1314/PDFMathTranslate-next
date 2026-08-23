"""Validate the copy-only Compose contract without starting containers."""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
COMPOSE = REPO_ROOT / "compose.yml"
README = REPO_ROOT / "README.md"
ACCEPTANCE = REPO_ROOT / "deploy" / "compose.acceptance.yml"
FORBIDDEN_WEB_ENV = {
    "PDF2ZH_PUBLIC_IP",
    "PDF2ZH_PUBLIC_PORT",
    "PDF2ZH_PUBLIC_ORIGIN",
    "PDF2ZH_SECURE_COOKIES",
    "PDF2ZH_TRUST_X_FORWARDED_FOR",
}


def run(*command: str, environment: dict[str, str] | None = None) -> str:
    completed = subprocess.run(
        command,
        cwd=REPO_ROOT,
        env=environment,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    )
    return completed.stdout


def compose_config(
    *files: Path, environment: dict[str, str] | None = None
) -> dict[str, object]:
    command = ["docker", "compose"]
    for file in files:
        command.extend(("-f", str(file)))
    command.extend(("config", "--format", "json"))
    return json.loads(run(*command, environment=environment))


def assert_root_compose(config: dict[str, object]) -> None:
    services = config["services"]
    assert isinstance(services, dict)
    assert set(services) == {"init", "translator", "web"}, services
    assert all("build" not in service for service in services.values())
    assert services["init"]["network_mode"] == "none"

    ports = services["web"]["ports"]
    assert len(ports) == 1, ports
    assert ports[0]["target"] == 7860, ports
    assert str(ports[0]["published"]) == "8199", ports
    environment = services["web"].get("environment", {})
    assert FORBIDDEN_WEB_ENV.isdisjoint(environment), environment


def assert_readme_copy() -> None:
    readme = README.read_text(encoding="utf-8")
    begin = "<!-- BEGIN PORTABLE COMPOSE -->"
    end = "<!-- END PORTABLE COMPOSE -->"
    assert readme.count(begin) == 1
    assert readme.count(end) == 1
    embedded = readme.split(begin, 1)[1].split(end, 1)[0].strip()
    assert embedded.startswith("```yaml\n") and embedded.endswith("\n```")
    embedded = embedded.removeprefix("```yaml\n").removesuffix("\n```")
    assert embedded + "\n" == COMPOSE.read_text(encoding="utf-8")


def assert_acceptance_overlay() -> None:
    with tempfile.TemporaryDirectory(prefix="pdf2zh-acceptance-") as directory:
        root = Path(directory).resolve()
        environment = os.environ.copy()
        environment["PDF2ZH_ACCEPTANCE_ROOT"] = str(root)
        config = compose_config(COMPOSE, ACCEPTANCE, environment=environment)
        services = config["services"]
        assert isinstance(services, dict)
        ports = services["web"]["ports"]
        assert len(ports) == 1 and str(ports[0]["published"]) == "17860", ports
        sources = []
        for service in services.values():
            for volume in service.get("volumes", []):
                if volume.get("type") == "bind":
                    sources.append(Path(volume["source"]).resolve())
        assert sources
        assert all(source.is_relative_to(root) for source in sources), sources


def main() -> None:
    assert_root_compose(compose_config(COMPOSE))
    assert_readme_copy()
    assert_acceptance_overlay()
    run("sh", "-n", str(REPO_ROOT / "deploy" / "portable-init.sh"))
    print("portable_compose_contract=passed", flush=True)


if __name__ == "__main__":
    main()
