pub async fn get_exchange_rate(
    from_code: &str,
    to_code: &str,
    date: &str,
) -> anyhow::Result<f64> {
    if from_code == to_code {
        return Ok(1.0);
    }
    let url = format!(
        "https://api.frankfurter.app/{}?from={}&to={}",
        date, from_code, to_code
    );
    let client = reqwest::Client::new();
    let resp: serde_json::Value = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?
        .json()
        .await?;
    resp["rates"][to_code]
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("Rate for {} not found in Frankfurter response", to_code))
}
