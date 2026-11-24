#!/usr/bin/env python3
"""BaiDec x BaiCode continuous optimization flywheel: report, curate SFT/eval sets, and calibrate thresholds.

Consumes trajectory JSONL files written by BaiCode (`$BAICODE_HOME/baidec/trajectories/YYYY/MM/DD/*.jsonl`)
and produces:
  1. `report`    — latency, shadow/enforce agreement, per-tool execution success, and failure diagnostics.
  2. `curate`    — verified `Intern-Decision` SFT training rows (`intern_decision_sft.jsonl`),
                   `BaiDec` native regression cases (`baidec_replay_cases.json`), and counterfactual
                   failure-to-recovery relabeled examples (`review_queue.jsonl`).
  3. `calibrate` — ECE and optimal `[baidec]` threshold recommendations (`config.toml` snippet).
"""

from __future__ import annotations

import argparse
from collections import Counter, defaultdict
import hashlib
import json
from pathlib import Path
import statistics
import sys
from typing import Any

VERIFICATION_MARKERS = (
    "cargo test",
    "cargo check",
    "cargo clippy",
    "cargo build",
    "pytest",
    "unittest",
    "npm test",
    "pnpm test",
    "yarn test",
    "go test",
    "ctest",
    "bazel test",
    "just test",
    "ruff check",
    "mypy",
    "tsc",
)


def default_trajectory_dir() -> Path:
    import os

    home = Path(os.environ.get("BAICODE_HOME", Path.home() / ".BaiCode"))
    return home / "baidec" / "trajectories"


def load_trajectory_events(root: Path) -> tuple[dict[str, dict[str, Any]], dict[str, dict[str, Any]]]:
    """Load and index `decision_step` and `step_outcome` events by `decision_id`."""
    steps: dict[str, dict[str, Any]] = {}
    outcomes: dict[str, dict[str, Any]] = {}
    if not root.exists():
        return steps, outcomes

    files = [root] if root.is_file() else sorted(root.rglob("*.jsonl"))
    for file_path in files:
        for raw_line in file_path.read_text(encoding="utf-8").splitlines():
            line = raw_line.strip()
            if not line:
                continue
            record = json.loads(line)
            decision_id = record.get("decision_id")
            if not decision_id:
                continue
            kind = record.get("kind")
            if kind == "decision_step":
                steps[decision_id] = record
            elif kind == "step_outcome":
                outcomes[decision_id] = record
    return steps, outcomes


def group_joined_turns(
    steps: dict[str, dict[str, Any]],
    outcomes: dict[str, dict[str, Any]],
) -> dict[tuple[str, str], list[dict[str, Any]]]:
    turns: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)
    for decision_id, step in steps.items():
        outcome = outcomes.get(decision_id)
        joined = {"step": step, "outcome": outcome}
        key = (str(step.get("thread_id", "")), str(step.get("turn_id", "")))
        turns[key].append(joined)
    for key in turns:
        turns[key].sort(key=lambda item: (int(item["step"].get("step_index", 0)), str(item["step"].get("timestamp", ""))))
    return turns


def extract_code_map_mode_from_outcome(outcome: dict[str, Any]) -> str | None:
    for call in outcome.get("observed_tool_calls") or []:
        if call.get("tool_name") == "code_map":
            raw_args = call.get("arguments_preview") or "{}"
            try:
                parsed = json.loads(raw_args)
                mode = parsed.get("mode", "repo_map")
                if mode in {"repo_map", "file_outline", "find_references"}:
                    return str(mode)
            except ValueError:
                return "repo_map"
    return None


def step_ran_verification(outcome: dict[str, Any] | None) -> bool:
    if not outcome:
        return False
    for call in outcome.get("observed_tool_calls") or []:
        if call.get("tool_name") in {"exec_command", "shell", "local_shell"}:
            args_lower = str(call.get("arguments_preview", "")).lower()
            if any(marker in args_lower for marker in VERIFICATION_MARKERS):
                return True
    return False


def build_report(steps: dict[str, dict[str, Any]], outcomes: dict[str, dict[str, Any]]) -> dict[str, Any]:
    latencies = [float(s["latency_ms"]) for s in steps.values() if s.get("latency_ms") is not None]
    degraded_reasons = Counter(
        str(s.get("routing_plan", {}).get("degraded_reason"))
        for s in steps.values()
        if s.get("routing_plan", {}).get("degraded_reason")
    )
    mode_counts = Counter(str(s.get("routing_plan", {}).get("mode", "unknown")) for s in steps.values())
    predicted_tools = Counter(str(s.get("routing_plan", {}).get("primary_tool", "unknown")) for s in steps.values())
    code_map_modes = Counter(
        str(s.get("routing_plan", {}).get("code_map_mode_hint"))
        for s in steps.values()
        if s.get("routing_plan", {}).get("code_map_mode_hint")
    )

    joined_count = 0
    action_agreements = 0
    tool_agreements = 0
    tool_success_by_name: dict[str, list[bool]] = defaultdict(list)

    for decision_id, step in steps.items():
        outcome = outcomes.get(decision_id)
        if not outcome:
            continue
        joined_count += 1
        if outcome.get("action_agreed"):
            action_agreements += 1
        if outcome.get("tool_agreed"):
            tool_agreements += 1
        for call in outcome.get("observed_tool_calls") or []:
            tool_success_by_name[str(call.get("tool_name", "unknown"))].append(bool(call.get("succeeded")))

    latencies_sorted = sorted(latencies)
    p50 = statistics.median(latencies_sorted) if latencies_sorted else 0.0
    p95_idx = min(len(latencies_sorted) - 1, int(len(latencies_sorted) * 0.95)) if latencies_sorted else 0
    p95 = latencies_sorted[p95_idx] if latencies_sorted else 0.0

    per_tool_stats = {
        tool: {
            "calls": len(results),
            "success_rate": round(sum(results) / len(results), 4) if results else 0.0,
        }
        for tool, results in sorted(tool_success_by_name.items())
    }

    return {
        "total_decision_steps": len(steps),
        "joined_step_outcomes": joined_count,
        "modes": dict(mode_counts),
        "latency_ms": {
            "p50": round(p50, 2),
            "p95": round(p95, 2),
            "mean": round(statistics.mean(latencies_sorted), 2) if latencies_sorted else 0.0,
        },
        "degraded_count": sum(degraded_reasons.values()),
        "degraded_reasons": dict(degraded_reasons),
        "agreement": {
            "action_agreement_rate": round(action_agreements / joined_count, 4) if joined_count else 0.0,
            "tool_agreement_rate": round(tool_agreements / joined_count, 4) if joined_count else 0.0,
        },
        "predicted_primary_tools": dict(predicted_tools),
        "code_map_mode_hints": dict(code_map_modes),
        "observed_tool_execution": per_tool_stats,
    }


def _sanitize_decision_marker(obj: Any) -> Any:
    if isinstance(obj, str):
        return obj.replace("<decision>", "< decision >")
    if isinstance(obj, list):
        return [_sanitize_decision_marker(v) for v in obj]
    if isinstance(obj, dict):
        return {_sanitize_decision_marker(k): _sanitize_decision_marker(v) for k, v in obj.items()}
    return obj


def scope_request_hash(request: dict[str, Any]) -> str:
    """Match `ai-infra/modules/baidec/tools/scope_replay_cases.py::request_hash` exactly."""
    order = [
        (field, list(q["criteria"]) if isinstance(q.get("criteria"), dict) else None)
        for field, q in (request.get("questions") or {}).items()
    ]
    encoded = json.dumps([request, order], ensure_ascii=False, separators=(",", ":"), sort_keys=True)
    return hashlib.sha256(encoded.encode("utf-8")).hexdigest()


def _state_question_fingerprint(request: dict[str, Any]) -> str:
    canonical = json.dumps(
        {
            "state": request.get("state"),
            "questions": list((request.get("questions") or {}).keys()),
        },
        ensure_ascii=False,
        sort_keys=True,
    )
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()[:16]


def curate_datasets(
    steps: dict[str, dict[str, Any]],
    outcomes: dict[str, dict[str, Any]],
) -> tuple[list[dict[str, Any]], list[dict[str, Any]], list[dict[str, Any]]]:
    """Curate Intern-Decision SFT rows, BaiDec replay cases, and review queue items."""
    turns = group_joined_turns(steps, outcomes)
    sft_rows: list[dict[str, Any]] = []
    replay_cases: list[dict[str, Any]] = []
    review_queue: list[dict[str, Any]] = []
    seen_fingerprints: set[str] = set()

    for (thread_id, turn_id), sequence in turns.items():
        if not sequence:
            continue
        last_outcome = sequence[-1]["outcome"]
        turn_completed_cleanly = bool(
            last_outcome
            and not last_outcome.get("sampling_error")
            and (last_outcome.get("produced_final_answer") or last_outcome.get("all_tools_succeeded"))
        )

        for idx, item in enumerate(sequence):
            step = item["step"]
            outcome = item["outcome"]
            request = step.get("request")
            if not isinstance(request, dict) or not outcome:
                continue
            questions = request.get("questions")
            if not isinstance(questions, dict) or not questions:
                continue

            actual_action = outcome.get("actual_action")
            actual_tool = outcome.get("actual_primary_tool")
            label_source = "verified_execution"
            chosen_action: str | None = None
            chosen_tool: str | None = None
            chosen_cm_mode: str | None = None

            if actual_action == "call_tool" and outcome.get("all_tools_succeeded") and turn_completed_cleanly:
                chosen_action = "call_tool"
                chosen_tool = str(actual_tool)
                chosen_cm_mode = extract_code_map_mode_from_outcome(outcome)
            elif actual_action == "final_answer" and turn_completed_cleanly and not step.get("has_unverified_edit"):
                chosen_action = "final_answer"
                chosen_tool = "none"
            elif actual_action == "call_tool" and not outcome.get("all_tools_succeeded"):
                # Counterfactual hindsight repair: check if step idx + 1 immediately recovered cleanly!
                if idx + 1 < len(sequence):
                    next_outcome = sequence[idx + 1]["outcome"]
                    if (
                        next_outcome
                        and next_outcome.get("actual_action") == "call_tool"
                        and next_outcome.get("all_tools_succeeded")
                    ):
                        repair_tool = str(next_outcome.get("actual_primary_tool", ""))
                        primary_criteria = (questions.get("primary_tool") or {}).get("criteria") or {}
                        if repair_tool in primary_criteria and repair_tool != actual_tool:
                            chosen_action = "call_tool"
                            chosen_tool = repair_tool
                            chosen_cm_mode = extract_code_map_mode_from_outcome(next_outcome)
                            label_source = "counterfactual_repair"

                if chosen_action is None:
                    review_queue.append(
                        {
                            "thread_id": thread_id,
                            "turn_id": turn_id,
                            "decision_id": step.get("decision_id"),
                            "step_index": step.get("step_index"),
                            "reason": "unrecovered_tool_failure",
                            "request": request,
                            "outcome": outcome,
                        }
                    )
                    continue
            else:
                continue

            # Validate candidate membership against the exact request criteria.
            primary_criteria = (questions.get("primary_tool") or {}).get("criteria") or {}
            if chosen_tool not in primary_criteria:
                continue

            future_ran_verification = any(
                step_ran_verification(later["outcome"]) for later in sequence[idx:]
            )
            needs_verif_label = "yes" if (step.get("has_unverified_edit") or future_ran_verification) else "no"

            clean_state = _sanitize_decision_marker(request["state"])
            clean_questions = _sanitize_decision_marker(questions)
            clean_request = {"state": clean_state, "questions": clean_questions}

            fp = _state_question_fingerprint(clean_request)
            if fp in seen_fingerprints:
                continue
            seen_fingerprints.add(fp)

            labeled_questions: dict[str, Any] = {}
            ground_truth: dict[str, str] = {}
            for q_name, q_spec in clean_questions.items():
                q_copy = dict(q_spec)
                if q_name == "turn_action":
                    label = chosen_action
                elif q_name == "primary_tool":
                    label = chosen_tool
                elif q_name == "code_map_mode":
                    label = chosen_cm_mode or str(
                        step.get("routing_plan", {}).get("code_map_mode_hint") or "repo_map"
                    )
                    if label not in ((q_spec.get("criteria") or {}).keys()):
                        label = "repo_map"
                elif q_name == "needs_verification":
                    label = needs_verif_label
                else:
                    continue
                q_copy["answer"] = {"label": label}
                labeled_questions[q_name] = q_copy
                ground_truth[q_name] = label

            resp_answers = ((step.get("response") or {}).get("answers") or {}) if isinstance(step.get("response"), dict) else {}
            replay_baseline = {}
            for field, gold_label in ground_truth.items():
                native_pred = (resp_answers.get(field) or {}).get("decision") or gold_label
                replay_baseline[field] = {
                    "gold": gold_label,
                    "native": str(native_pred),
                    "hf": gold_label,
                }

            sample_id = f"baicode-{fp}"
            sft_rows.append(
                {
                    "id": sample_id,
                    "source": label_source,
                    "generator_model": step.get("generator_model"),
                    "state": clean_state,
                    "questions": labeled_questions,
                    "targets": {k: {"label": v} for k, v in ground_truth.items()},
                }
            )
            replay_cases.append(
                {
                    "id": sample_id,
                    "suite": "baicode",
                    "line": len(replay_cases) + 1,
                    "source": label_source,
                    "request": clean_request,
                    "request_hash": scope_request_hash(clean_request),
                    "ground_truth": ground_truth,
                    "baseline": replay_baseline,
                }
            )

    return sft_rows, replay_cases, review_queue


def calibrate_thresholds(
    steps: dict[str, dict[str, Any]],
    outcomes: dict[str, dict[str, Any]],
    min_recall: float = 0.85,
) -> dict[str, Any]:
    """Compute ECE and sweep confidence thresholds on joined step outcomes."""
    tool_samples: list[tuple[float, bool]] = []
    finish_samples: list[tuple[float, bool]] = []

    for decision_id, step in steps.items():
        outcome = outcomes.get(decision_id)
        resp = step.get("response")
        if not outcome or not isinstance(resp, dict):
            continue
        answers = resp.get("answers") or {}

        t_ans = answers.get("primary_tool") or {}
        if outcome.get("actual_action") == "call_tool" and outcome.get("all_tools_succeeded"):
            conf = float(t_ans.get("confidence", 0.0))
            correct = str(t_ans.get("decision")) == str(outcome.get("actual_primary_tool"))
            tool_samples.append((conf, correct))

        a_ans = answers.get("turn_action") or {}
        if str(a_ans.get("decision")) == "final_answer":
            conf = float(a_ans.get("confidence", 0.0))
            correct = str(outcome.get("actual_action")) == "final_answer"
            finish_samples.append((conf, correct))

    def compute_ece(samples: list[tuple[float, bool]], bins: int = 10) -> float:
        if not samples:
            return 0.0
        n = len(samples)
        ece = 0.0
        for b in range(bins):
            lo, hi = b / bins, (b + 1) / bins
            bucket = [s for s in samples if (lo <= s[0] < hi or (b == bins - 1 and s[0] == 1.0))]
            if not bucket:
                continue
            avg_conf = sum(s[0] for s in bucket) / len(bucket)
            acc = sum(1.0 for s in bucket if s[1]) / len(bucket)
            ece += (len(bucket) / n) * abs(avg_conf - acc)
        return round(ece, 4)

    def pick_threshold(samples: list[tuple[float, bool]], default_val: float) -> tuple[float, float, float]:
        if len(samples) < 3:
            return default_val, 1.0, 1.0
        total_pos = sum(1 for _, ok in samples if ok)
        if total_pos == 0:
            return default_val, 0.0, 0.0
        best = (default_val, 0.0, 0.0)
        for step_i in range(20, 91, 5):
            th = round(step_i / 100.0, 2)
            kept = [ok for conf, ok in samples if conf >= th]
            if not kept:
                continue
            tp = sum(1 for ok in kept if ok)
            precision = tp / len(kept)
            recall = tp / total_pos
            if recall >= min_recall and precision >= best[1]:
                best = (th, round(precision, 4), round(recall, 4))
        if best[1] == 0.0:
            return default_val, 0.0, 0.0
        return best

    tool_th, tool_prec, tool_rec = pick_threshold(tool_samples, 0.30)
    act_th, act_prec, act_rec = pick_threshold(finish_samples, 0.60)

    toml_snippet = (
        "[baidec]\n"
        "mode = \"enforce\"\n"
        "url = \"http://127.0.0.1:18190/v1/decisions\"\n"
        f"min_action_confidence = {act_th:.2f}\n"
        f"min_tool_confidence = {tool_th:.2f}\n"
        "secondary_tool_threshold = 0.18\n"
        "max_routed_tools = 3\n"
        "record_trajectories = true\n"
    )
    return {
        "tool_samples": len(tool_samples),
        "finish_samples": len(finish_samples),
        "primary_tool_ece": compute_ece(tool_samples),
        "turn_action_ece": compute_ece(finish_samples),
        "recommended": {
            "min_tool_confidence": tool_th,
            "tool_precision": tool_prec,
            "tool_recall": tool_rec,
            "min_action_confidence": act_th,
            "action_precision": act_prec,
            "action_recall": act_rec,
        },
        "recommended_config_toml": toml_snippet,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--trajectories",
        type=Path,
        default=default_trajectory_dir(),
        help="Trajectory directory or JSONL file (default: $BAICODE_HOME/baidec/trajectories)",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("report", help="Print latency, agreement, and tool execution metrics")

    curate_p = sub.add_parser(
        "curate",
        help="Export Intern-Decision SFT dataset, BaiDec replay cases, and counterfactual review queue",
    )
    curate_p.add_argument("--out-dir", type=Path, required=True, help="Output directory for curated artifacts")

    cal_p = sub.add_parser("calibrate", help="Compute ECE and recommend optimal [baidec] thresholds")
    cal_p.add_argument("--min-recall", type=float, default=0.85, help="Minimum recall constraint (default: 0.85)")

    args = parser.parse_args(argv)
    steps, outcomes = load_trajectory_events(args.trajectories)

    if args.command == "report":
        print(json.dumps(build_report(steps, outcomes), ensure_ascii=False, indent=2))
        return 0

    if args.command == "curate":
        sft_rows, replay_cases, review_queue = curate_datasets(steps, outcomes)
        out_dir: Path = args.out_dir
        out_dir.mkdir(parents=True, exist_ok=True)
        sft_path = out_dir / "intern_decision_sft.jsonl"
        replay_path = out_dir / "baidec_replay_cases.json"
        review_path = out_dir / "review_queue.jsonl"

        sft_path.write_text(
            "".join(json.dumps(row, ensure_ascii=False) + "\n" for row in sft_rows),
            encoding="utf-8",
        )
        replay_path.write_text(
            json.dumps(
                {"schema": 1, "selection": "baicode-curated", "cases": replay_cases},
                ensure_ascii=False,
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
        review_path.write_text(
            "".join(json.dumps(item, ensure_ascii=False) + "\n" for item in review_queue),
            encoding="utf-8",
        )
        summary = {
            "sft_rows": len(sft_rows),
            "counterfactual_repairs": sum(1 for r in sft_rows if r["source"] == "counterfactual_repair"),
            "replay_cases": len(replay_cases),
            "review_queue": len(review_queue),
            "files": {
                "intern_decision_sft": str(sft_path),
                "baidec_replay_cases": str(replay_path),
                "review_queue": str(review_path),
            },
        }
        print(json.dumps(summary, ensure_ascii=False, indent=2))
        return 0

    if args.command == "calibrate":
        result = calibrate_thresholds(steps, outcomes, min_recall=args.min_recall)
        print(json.dumps(result, ensure_ascii=False, indent=2))
        return 0

    return 1


if __name__ == "__main__":
    raise SystemExit(main())
