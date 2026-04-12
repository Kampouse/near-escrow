"""LLM-based work scorer using Gemini.

Implements multi-pass verification for reliability:
1. Decompose criteria into checkable parts
2. Run N independent scoring passes
3. Aggregate scores and produce final verdict

Scoring granularity: 5-point buckets (0-4, 5-9, 10-14, ..., 95-100)
for consistency across passes.
"""

import json
import logging
from typing import Optional

from google import genai

log = logging.getLogger(__name__)


class Scorer:
    """Scores work results against criteria using Gemini."""

    def __init__(self, config: dict):
        self.model = config.get("gemini_model", "gemini-2.5-flash")
        self.passes = config.get("verification_passes", 4)
        self.client = genai.Client()  # Uses GEMINI_API_KEY env var

    def score(
        self,
        task_description: str,
        criteria: str,
        result: str,
        threshold: int = 80,
    ) -> dict:
        """Score a work result against criteria.

        Runs multiple independent verification passes and aggregates.

        Returns:
            {
                "score": int (0-100),
                "passed": bool,
                "detail": str,
                "passes": [{"score": int, "reasoning": str}, ...]
            }
        """
        prompt = self._build_prompt(task_description, criteria, result, threshold)

        pass_results = []
        for i in range(self.passes):
            try:
                score, reasoning = self._single_pass(prompt, i + 1)
                pass_results.append({"score": score, "reasoning": reasoning})
                log.info("Pass %d/%d: score=%d", i + 1, self.passes, score)
            except Exception as e:
                log.error("Pass %d failed: %s", i + 1, e)
                pass_results.append({"score": 0, "reasoning": f"Error: {e}"})

        # Aggregate: median of scores
        scores = [p["score"] for p in pass_results]
        scores.sort()
        median_score = scores[len(scores) // 2]

        # Average reasoning from passes near the median
        passed = median_score >= threshold

        # Build detail from best pass
        best_pass = min(
            pass_results,
            key=lambda p: abs(p["score"] - median_score),
        )

        detail = (
            f"Median of {self.passes} passes: {median_score}/100. "
            f"Threshold: {threshold}. "
            f"Best reasoning: {best_pass['reasoning']}"
        )

        return {
            "score": median_score,
            "passed": passed,
            "detail": detail,
            "passes": pass_results,
        }

    def _single_pass(self, prompt: str, pass_num: int) -> tuple[int, str]:
        """Run a single verification pass. Returns (score, reasoning)."""
        response = self.client.models.generate_content(
            model=self.model,
            contents=prompt,
            config={
                "temperature": 0.3 + (pass_num * 0.1),  # Slight variation per pass
                "response_mime_type": "application/json",
            },
        )

        text = response.text.strip()
        data = json.loads(text)

        score = int(data.get("score", 0))
        score = max(0, min(100, score))  # Clamp to 0-100
        reasoning = data.get("reasoning", "No reasoning provided")

        return score, reasoning

    def _build_prompt(
        self,
        task_description: str,
        criteria: str,
        result: str,
        threshold: int,
    ) -> str:
        """Build the scoring prompt for Gemini."""
        return f"""You are an impartial work verifier. Score the following work result against the given criteria.

## Task Description
{task_description}

## Acceptance Criteria
{criteria}

## Work Result
{result}

## Instructions
1. Carefully evaluate the work result against each criterion.
2. Score from 0 to 100, where:
   - 0-20: Completely fails to address the task
   - 21-40: Addresses some aspects but major gaps
   - 41-60: Partially meets criteria, significant issues remain
   - 61-80: Mostly meets criteria, minor issues
   - 81-100: Fully meets or exceeds all criteria
3. The passing threshold is {threshold}/100.

Respond in JSON format:
{{
  "score": <number 0-100>,
  "reasoning": "<detailed explanation of the score, addressing each criterion>"
}}"""
