"""Validate evaluation requests and hold question schemas."""

from __future__ import annotations

from dataclasses import dataclass

from .constants import (
    DEFAULT_MAX_INPUT_TOKENS,
    HARD_MAX_INPUT_TOKENS,
    MAX_DESCRIPTION_CHARS,
    MAX_ID_CHARS,
    MAX_LABEL_CHARS,
    MAX_LABELS,
    MAX_PAIRS,
    MAX_QUESTIONS,
    MAX_STATEMENT_CHARS,
    MAX_STATES,
    MAX_TEXT_CHARS,
    RESERVED_MARKERS,
)


class ProtocolError(ValueError):
    """The request frame parsed but does not match the worker contract."""


@dataclass(frozen=True)
class Label:
    name: str
    description: str | None


@dataclass(frozen=True)
class ChoiceQuestion:
    id: str
    labels: tuple[Label, ...]


@dataclass(frozen=True)
class Level:
    name: str
    description: str | None
    value: float


@dataclass(frozen=True)
class OrdinalQuestion:
    id: str
    levels: tuple[Level, ...]


@dataclass(frozen=True)
class PredicateQuestion:
    id: str
    statement: str


Question = ChoiceQuestion | OrdinalQuestion | PredicateQuestion


@dataclass(frozen=True)
class State:
    id: str
    text: str


@dataclass(frozen=True)
class EvaluateRequest:
    id: str
    states: tuple[State, ...]
    questions: tuple[Question, ...]
    max_input_tokens: int


def label_names(question: ChoiceQuestion | OrdinalQuestion) -> tuple[str, ...]:
    if isinstance(question, ChoiceQuestion):
        return tuple(label.name for label in question.labels)
    return tuple(level.name for level in question.levels)


def parse_evaluate(payload: dict) -> EvaluateRequest:
    _reject_unknown(
        payload,
        {"version", "id", "op", "states", "questions", "max_input_tokens"},
        "evaluate",
    )
    request_id = _identifier(payload.get("id"), "id")
    states_raw = payload.get("states")
    questions_raw = payload.get("questions")
    if not isinstance(states_raw, list) or not states_raw:
        raise ProtocolError("states must be a non-empty list")
    if not isinstance(questions_raw, list) or not questions_raw:
        raise ProtocolError("questions must be a non-empty list")
    if len(states_raw) > MAX_STATES:
        raise ProtocolError(f"states exceeds limit {MAX_STATES}")
    if len(questions_raw) > MAX_QUESTIONS:
        raise ProtocolError(f"questions exceeds limit {MAX_QUESTIONS}")
    if len(states_raw) * len(questions_raw) > MAX_PAIRS:
        raise ProtocolError(f"state-question pairs exceed queue limit {MAX_PAIRS}")
    if "max_input_tokens" in payload and payload["max_input_tokens"] is not None:
        limit = _bounded_int(
            payload["max_input_tokens"],
            "max_input_tokens",
            1,
            HARD_MAX_INPUT_TOKENS,
        )
    else:
        limit = DEFAULT_MAX_INPUT_TOKENS
    states = tuple(_parse_state(item, index) for index, item in enumerate(states_raw))
    questions = tuple(
        _parse_question(item, index) for index, item in enumerate(questions_raw)
    )
    state_ids = [state.id for state in states]
    question_ids = [question.id for question in questions]
    if len(set(state_ids)) != len(state_ids):
        raise ProtocolError("state ids must be unique")
    if len(set(question_ids)) != len(question_ids):
        raise ProtocolError("question ids must be unique")
    return EvaluateRequest(
        id=request_id,
        states=states,
        questions=questions,
        max_input_tokens=limit,
    )


def parse_shutdown(payload: dict) -> str:
    _reject_unknown(payload, {"version", "id", "op"}, "shutdown")
    return _identifier(payload.get("id"), "id")


def _parse_state(item: object, index: int) -> State:
    if not isinstance(item, dict):
        raise ProtocolError(f"states[{index}] must be an object")
    _reject_unknown(item, {"id", "text"}, f"states[{index}]")
    return State(
        id=_identifier(item.get("id"), f"states[{index}].id"),
        text=_text(item.get("text"), f"states[{index}].text"),
    )


def _parse_question(item: object, index: int) -> Question:
    if not isinstance(item, dict):
        raise ProtocolError(f"questions[{index}] must be an object")
    kind = item.get("kind")
    where = f"questions[{index}]"
    if kind == "choice":
        _reject_unknown(item, {"kind", "id", "labels"}, where)
        return ChoiceQuestion(
            id=_task_name(item.get("id"), f"{where}.id"),
            labels=_parse_labels(item.get("labels"), where),
        )
    if kind == "ordinal":
        _reject_unknown(item, {"kind", "id", "levels"}, where)
        return OrdinalQuestion(
            id=_task_name(item.get("id"), f"{where}.id"),
            levels=_parse_levels(item.get("levels"), where),
        )
    if kind == "predicate":
        _reject_unknown(item, {"kind", "id", "statement"}, where)
        return PredicateQuestion(
            id=_task_name(item.get("id"), f"{where}.id"),
            statement=_statement(item.get("statement"), f"{where}.statement"),
        )
    raise ProtocolError(f"{where}.kind must be choice, ordinal, or predicate")


def _parse_labels(raw: object, where: str) -> tuple[Label, ...]:
    rows = _label_rows(raw, f"{where}.labels")
    labels = []
    for index, row in enumerate(rows):
        _reject_unknown(row, {"label", "description"}, f"{where}.labels[{index}]")
        labels.append(
            Label(
                name=_label_name(row.get("label"), f"{where}.labels[{index}].label"),
                description=_optional_description(
                    row, f"{where}.labels[{index}].description"
                ),
            )
        )
    _unique_names([label.name for label in labels], where)
    return tuple(labels)


def _parse_levels(raw: object, where: str) -> tuple[Level, ...]:
    rows = _label_rows(raw, f"{where}.levels")
    if len(rows) < 2:
        raise ProtocolError(f"{where}.levels requires at least two levels")
    levels = []
    for index, row in enumerate(rows):
        _reject_unknown(
            row, {"label", "description", "value"}, f"{where}.levels[{index}]"
        )
        if "value" not in row:
            raise ProtocolError(f"{where}.levels[{index}].value is required")
        levels.append(
            Level(
                name=_label_name(row.get("label"), f"{where}.levels[{index}].label"),
                description=_optional_description(
                    row, f"{where}.levels[{index}].description"
                ),
                value=_finite_number(row.get("value"), f"{where}.levels[{index}].value"),
            )
        )
    _unique_names([level.name for level in levels], where)
    return tuple(levels)


def _label_rows(raw: object, where: str) -> list[dict]:
    if not isinstance(raw, list) or not raw:
        raise ProtocolError(f"{where} must be a non-empty list")
    if len(raw) > MAX_LABELS:
        raise ProtocolError(f"{where} exceeds limit {MAX_LABELS}")
    rows = []
    for index, row in enumerate(raw):
        if not isinstance(row, dict):
            raise ProtocolError(f"{where}[{index}] must be an object")
        rows.append(row)
    return rows


def _reject_unknown(payload: dict, allowed: set[str], where: str) -> None:
    unknown = sorted(set(payload) - allowed)
    if unknown:
        raise ProtocolError(f"{where} has unknown fields: {', '.join(unknown)}")


def _identifier(value: object, where: str) -> str:
    text = _plain_string(value, where, MAX_ID_CHARS)
    _reject_markers(text, where)
    return text


def _task_name(value: object, where: str) -> str:
    return _identifier(value, where)


def _label_name(value: object, where: str) -> str:
    text = _plain_string(value, where, MAX_LABEL_CHARS)
    _reject_markers(text, where)
    return text


def _text(value: object, where: str) -> str:
    if not isinstance(value, str):
        raise ProtocolError(f"{where} must be a string")
    if len(value) > MAX_TEXT_CHARS:
        raise ProtocolError(f"{where} exceeds {MAX_TEXT_CHARS} characters")
    return value


def _statement(value: object, where: str) -> str:
    text = _plain_string(value, where, MAX_STATEMENT_CHARS)
    _reject_markers(text, where)
    return text


def _optional_description(row: dict, where: str) -> str | None:
    if "description" not in row or row["description"] is None:
        return None
    text = _plain_string(row["description"], where, MAX_DESCRIPTION_CHARS)
    _reject_markers(text, where)
    return text


def _plain_string(value: object, where: str, limit: int) -> str:
    if not isinstance(value, str):
        raise ProtocolError(f"{where} must be a string")
    if value.strip() != value or not value:
        raise ProtocolError(f"{where} must be a non-empty string without surrounding whitespace")
    if len(value) > limit:
        raise ProtocolError(f"{where} exceeds {limit} characters")
    if any(ord(char) < 32 for char in value):
        raise ProtocolError(f"{where} must not contain control characters")
    return value


def _reject_markers(value: str, where: str) -> None:
    for marker in RESERVED_MARKERS:
        if marker in value:
            raise ProtocolError(
                f"{where} contains reserved marker {marker!r} and would corrupt label alignment"
            )


def _unique_names(names: list[str], where: str) -> None:
    if len(set(names)) != len(names):
        raise ProtocolError(f"{where} has duplicate labels")


def _finite_number(value: object, where: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ProtocolError(f"{where} must be a finite number")
    number = float(value)
    if number != number or number in (float("inf"), float("-inf")):
        raise ProtocolError(f"{where} must be a finite number")
    return number


def _bounded_int(value: object, where: str, low: int, high: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ProtocolError(f"{where} must be an integer")
    if value < low or value > high:
        raise ProtocolError(f"{where} must be between {low} and {high}")
    return value
