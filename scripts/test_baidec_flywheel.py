#!/usr/bin/env python3
"""Unit tests for scripts/baidec_flywheel.py, including Intern-Decision compile_row compatibility."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SCRIPT_PATH = Path(__file__).with_name("baidec_flywheel.py")
spec = importlib.util.spec_from_file_location("baidec_flywheel", SCRIPT_PATH)
assert spec and spec.loader
flywheel = importlib.util.module_from_spec(spec)
spec.loader.exec_module(flywheel)


def _sample_questions() -> dict:
    return {
        "turn_action": {
            "type": "choice",
            "instructions": "Call tool or answer?",
            "criteria": {"call_tool": "Call tool", "final_answer": "Answer"},
        },
        "primary_tool": {
            "type": "choice",
            "instructions": "Select primary tool",
            "criteria": {
                "code_map": "Structural map",
                "apply_patch": "Edit file",
                "exec_command": "Run shell",
                "none": "No tool",
            },
        },
        "code_map_mode": {
            "type": "choice",
            "instructions": "Select code_map mode",
            "criteria": {
                "repo_map": "Workspace overview",
                "file_outline": "File symbols",
                "find_references": "Symbol refs",
            },
        },
        "needs_verification": {
            "type": "noul",
            "instructions": "Needs build/test verification?",
        },
    }


class BaiDecFlywheelTests(unittest.TestCase):
    def _write_sample_trajectory(self, directory: Path) -> None:
        records = [
            # Step 0: apply_patch failed initially because lines weren't checked -> step 1 repairs with code_map!
            {
                "kind": "decision_step",
                "thread_id": "t1",
                "turn_id": "turn1",
                "decision_id": "d0",
                "step_index": 0,
                "generator_model": "qwen3.6-35b-a3b-q4_k_m",
                "has_unverified_edit": False,
                "latency_ms": 158.4,
                "request": {
                    "state": {"user_goal": "Fix <decision> parse bug in parser.rs", "step_index": 0},
                    "questions": _sample_questions(),
                },
                "response": {
                    "answers": {
                        "turn_action": {"decision": "call_tool", "confidence": 0.91},
                        "primary_tool": {"decision": "apply_patch", "confidence": 0.62},
                    }
                },
                "routing_plan": {
                    "mode": "enforce",
                    "primary_tool": "apply_patch",
                    "code_map_mode_hint": "file_outline",
                },
            },
            {
                "kind": "step_outcome",
                "thread_id": "t1",
                "turn_id": "turn1",
                "decision_id": "d0",
                "step_index": 0,
                "actual_action": "call_tool",
                "actual_primary_tool": "apply_patch",
                "action_agreed": True,
                "tool_agreed": True,
                "all_tools_succeeded": False,
                "produced_final_answer": False,
                "observed_tool_calls": [
                    {
                        "call_id": "c0",
                        "tool_name": "apply_patch",
                        "arguments_preview": '{"input":"bad hunk"}',
                        "succeeded": False,
                        "exit_code": None,
                        "output_preview": "apply_patch verification failed",
                    }
                ],
            },
            # Step 1: recovers cleanly using code_map(file_outline)!
            {
                "kind": "decision_step",
                "thread_id": "t1",
                "turn_id": "turn1",
                "decision_id": "d1",
                "step_index": 1,
                "generator_model": "qwen3.6-35b-a3b-q4_k_m",
                "has_unverified_edit": False,
                "latency_ms": 162.1,
                "request": {
                    "state": {"user_goal": "Fix parse bug in parser.rs", "step_index": 1},
                    "questions": _sample_questions(),
                },
                "response": {
                    "answers": {
                        "turn_action": {"decision": "call_tool", "confidence": 0.95},
                        "primary_tool": {"decision": "code_map", "confidence": 0.88},
                    }
                },
                "routing_plan": {
                    "mode": "enforce",
                    "primary_tool": "code_map",
                    "code_map_mode_hint": "file_outline",
                },
            },
            {
                "kind": "step_outcome",
                "thread_id": "t1",
                "turn_id": "turn1",
                "decision_id": "d1",
                "step_index": 1,
                "actual_action": "call_tool",
                "actual_primary_tool": "code_map",
                "action_agreed": True,
                "tool_agreed": True,
                "all_tools_succeeded": True,
                "produced_final_answer": False,
                "observed_tool_calls": [
                    {
                        "call_id": "c1",
                        "tool_name": "code_map",
                        "arguments_preview": '{"mode":"file_outline","path":"src/parser.rs"}',
                        "succeeded": True,
                        "exit_code": None,
                        "output_preview": "12: fn parse()",
                    }
                ],
            },
            # Step 2: runs cargo test verification and succeeds
            {
                "kind": "decision_step",
                "thread_id": "t1",
                "turn_id": "turn1",
                "decision_id": "d2",
                "step_index": 2,
                "generator_model": "qwen3.6-35b-a3b-q4_k_m",
                "has_unverified_edit": True,
                "latency_ms": 160.0,
                "request": {
                    "state": {"user_goal": "Fix parse bug in parser.rs", "step_index": 2},
                    "questions": _sample_questions(),
                },
                "response": {
                    "answers": {
                        "turn_action": {"decision": "call_tool", "confidence": 0.96},
                        "primary_tool": {"decision": "exec_command", "confidence": 0.90},
                    }
                },
                "routing_plan": {
                    "mode": "enforce",
                    "primary_tool": "exec_command",
                },
            },
            {
                "kind": "step_outcome",
                "thread_id": "t1",
                "turn_id": "turn1",
                "decision_id": "d2",
                "step_index": 2,
                "actual_action": "call_tool",
                "actual_primary_tool": "exec_command",
                "action_agreed": True,
                "tool_agreed": True,
                "all_tools_succeeded": True,
                "produced_final_answer": True,
                "observed_tool_calls": [
                    {
                        "call_id": "c2",
                        "tool_name": "exec_command",
                        "arguments_preview": '{"cmd":"cargo test"}',
                        "succeeded": True,
                        "exit_code": 0,
                        "output_preview": "Exit code: 0",
                    }
                ],
            },
        ]
        out = directory / "decisions-t1.jsonl"
        out.write_text("".join(json.dumps(r) + "\n" for r in records), encoding="utf-8")

    def test_curate_and_counterfactual_repair(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._write_sample_trajectory(root)
            steps, outcomes = flywheel.load_trajectory_events(root)
            report = flywheel.build_report(steps, outcomes)
            self.assertEqual(report["total_decision_steps"], 3)
            self.assertEqual(report["joined_step_outcomes"], 3)

            sft_rows, replay_cases, review_queue = flywheel.curate_datasets(steps, outcomes)
            self.assertEqual(len(sft_rows), 3)
            self.assertEqual(len(replay_cases), 3)
            self.assertEqual(len(review_queue), 0)

            # d0 failed apply_patch and was counterfactually repaired to d1's winning code_map(file_outline)!
            repaired = next(r for r in sft_rows if r["source"] == "counterfactual_repair")
            self.assertEqual(repaired["targets"]["primary_tool"]["label"], "code_map")
            self.assertEqual(repaired["targets"]["code_map_mode"]["label"], "file_outline")
            # Because d2 ran `cargo test`, lookahead `needs_verification` is labeled `"yes"`!
            self.assertEqual(repaired["targets"]["needs_verification"]["label"], "yes")

            # If Intern-Decision's schema.py is present locally, verify 100% `compile_row` compatibility!
            schema_candidates = [
                Path("/home/ops396/xiaolt1/workspace/ai-infra/sources/intern-decision/src/inputs/schema.py"),
                Path("/home/ops396/xiaolt1/workspace/ai-infra/modules/baidec/sources/intern-decision/src/inputs/schema.py"),
            ]
            intern_schema = next((p for p in schema_candidates if p.is_file()), None)
            if intern_schema is not None:
                import sys

                s_spec = importlib.util.spec_from_file_location("intern_schema", intern_schema)
                assert s_spec and s_spec.loader
                schema_mod = importlib.util.module_from_spec(s_spec)
                sys.modules[s_spec.name] = schema_mod
                s_spec.loader.exec_module(schema_mod)
                for row in sft_rows:
                    compiled = schema_mod.compile_row(row, include_targets=True)
                    self.assertEqual(
                        list(compiled.fields),
                        ["turn_action", "primary_tool", "code_map_mode", "needs_verification"],
                    )
                    self.assertIsNotNone(compiled.targets)

            # Verify scope_replay.py pack schema & exact request_hash compatibility!
            scope_tools_dir = Path("/home/ops396/xiaolt1/workspace/ai-infra/modules/baidec/tools")
            if (scope_tools_dir / "scope_replay_cases.py").is_file():
                import sys

                if str(scope_tools_dir) not in sys.path:
                    sys.path.insert(0, str(scope_tools_dir))
                import scope_replay_cases

                for idx, case in enumerate(replay_cases, 1):
                    self.assertEqual(case["suite"], "baicode")
                    self.assertEqual(case["line"], idx)
                    self.assertEqual(
                        case["request_hash"],
                        scope_replay_cases.request_hash(case["request"]),
                    )
                    for field, verdict in case["baseline"].items():
                        self.assertIn("gold", verdict)
                        self.assertIn("native", verdict)
                        self.assertIn("hf", verdict)
                        self.assertEqual(verdict["gold"], case["ground_truth"][field])

            cal = flywheel.calibrate_thresholds(steps, outcomes)
            self.assertIn("[baidec]", cal["recommended_config_toml"])


if __name__ == "__main__":
    unittest.main()
