use anyhow::Result;
use serde::{Deserialize, Serialize};
use crate::config::Config;

/// LLM-based scorer — evaluates the worker's result against the task criteria.
pub struct Scorer {
    client: reqwest::Client,
    config: Config,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Verdict {
    pub score: u8,
    pub passed: bool,
    pub detail: String,
}

impl Scorer {
    pub fn new(config: Config) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
        }
    }

    /// Score a submission using the LLM.
    pub async fn score(&self, task: &str, criteria: &str, result: &str) -> Result<Verdict> {
        let prompt = format!(
r#"You are an escrow verifier. Score the worker's submission.

TASK: {task}

CRITERIA: {criteria}

WORKER SUBMISSION:
{result}

Score from 0-100. Be strict but fair.
Respond with ONLY valid JSON: {{"score": <0-100>, "detail": "<brief explanation>"}}
Do NOT include any other text."#
        );

        let response = self.client.post(&self.config.llm_url)
            .header("Authorization", format!("Bearer {}", self.config.llm_api_key))
            .json(&serde_json::json!({
                "model": self.config.llm_model,
                "messages": [{"role": "user", "content": prompt}],
                "temperature": 0.3,
                "max_tokens": 200,
            }))
            .send().await?
            .error_for_status()?;

        let body: serde_json::Value = response.json().await?;
        let content = body["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("No content in LLM response"))?;

        // Parse the LLM response
        let parsed: serde_json::Value = serde_json::from_str(content.trim())
            .map_err(|e| anyhow::anyhow!("Failed to parse LLM response as JSON: {} — content: {}", e, content))?;

        let score = parsed["score"].as_u64()
            .ok_or_else(|| anyhow::anyhow!("No 'score' in LLM response"))? as u8;

        let detail = parsed["detail"].as_str()
            .unwrap_or("No detail provided")
            .to_string();

        Ok(Verdict {
            score: score.min(100),
            passed: (score as u8) >= self.config.score_threshold,
            detail,
        })
    }
}
