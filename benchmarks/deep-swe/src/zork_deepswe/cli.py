from __future__ import annotations

import argparse

from zork_deepswe import compare, prepare, profile, responses_probe, run


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="zork-deep-swe",
        description="Run and inspect Zork's reproducible DeepSWE benchmark.",
    )
    commands = parser.add_subparsers(dest="command", required=True)
    for name, help_text, module in (
        ("run", "run the fixed DeepSWE subset", run),
        ("profile", "build a benchmark profile", profile),
        ("prepare", "pre-pull task images", prepare),
        ("compare", "compare benchmark result sets", compare),
        ("responses-probe", "record Responses protocol metadata", responses_probe),
    ):
        command = commands.add_parser(name, help=help_text)
        module.add_arguments(command)
        command.set_defaults(handler=module.execute)
    return parser


def main(argv: list[str] | None = None) -> None:
    parser = build_parser()
    arguments = parser.parse_args(argv)
    try:
        arguments.handler(arguments)
    except (FileNotFoundError, TypeError, ValueError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
