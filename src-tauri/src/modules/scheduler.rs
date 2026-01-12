use chrono::Utc;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::time::{self, Duration};
use crate::modules::{config, logger, quota, account};

// 预热历史记录：key = "email:model_name:100", value = 预热时间戳
static WARMUP_HISTORY: Lazy<Mutex<HashMap<String, i64>>> = Lazy::new(|| Mutex::new(HashMap::new()));

pub fn start_scheduler() {
    tauri::async_runtime::spawn(async {
        logger::log_info("Smart Warmup Scheduler started. Monitoring quota at 100%...");
        
        // 每 10 分钟扫描一次
        let mut interval = time::interval(Duration::from_secs(600));

        loop {
            interval.tick().await;

            // 加载配置
            let Ok(app_config) = config::load_app_config() else {
                continue;
            };

            if !app_config.scheduled_warmup.enabled {
                continue;
            }
            
            // 获取所有账号（不再过滤等级）
            let Ok(accounts) = account::list_accounts() else {
                continue;
            };

            if accounts.is_empty() {
                continue;
            }

            logger::log_info(&format!(
                "[Scheduler] Scanning {} accounts for 100% quota models...",
                accounts.len()
            ));

            let mut warmup_tasks = Vec::new();

            // 扫描每个账号的每个模型
            for account in &accounts {
                // 获取有效 token
                let Ok((token, pid)) = quota::get_valid_token_for_warmup(account).await else {
                    continue;
                };

                // 获取实时配额
                let Ok((fresh_quota, _)) = quota::fetch_quota_with_cache(&token, &account.email, Some(&pid)).await else {
                    continue;
                };

                let now_ts = Utc::now().timestamp();

                for model in fresh_quota.models {
                    let history_key = format!("{}:{}:100", account.email, model.name);
                    
                    // 核心逻辑：检测 100% 额度
                    if model.percentage == 100 {
                        // 检查是否已经在本周期预热过
                        let mut history = WARMUP_HISTORY.lock().unwrap();
                        if history.contains_key(&history_key) {
                            // 已经预热过这个 100% 周期，跳过
                            continue;
                        }

                        // 记录到历史
                        history.insert(history_key.clone(), now_ts);
                        drop(history);

                        // 模型名称映射
                        let model_to_ping = if model.name == "gemini-2.5-flash" {
                            "gemini-3-flash".to_string()
                        } else {
                            model.name.clone()
                        };

                        // 严格白名单过滤
                        match model_to_ping.as_str() {
                            "gemini-3-flash" | "claude-sonnet-4-5" | "gemini-3-pro-high" | "gemini-3-pro-image" => {
                                warmup_tasks.push((
                                    account.email.clone(),
                                    model_to_ping.clone(),
                                    token.clone(),
                                    pid.clone(),
                                    model.percentage,
                                ));

                                logger::log_info(&format!(
                                    "[Scheduler] ✓ Scheduled warmup: {} @ {} (quota at 100%)",
                                    model_to_ping, account.email
                                ));
                            }
                            _ => continue,
                        }
                    } else if model.percentage < 100 {
                        // 额度未满，清除历史记录，允许下次 100% 时再预热
                        let mut history = WARMUP_HISTORY.lock().unwrap();
                        if history.remove(&history_key).is_some() {
                            logger::log_info(&format!(
                                "[Scheduler] Cleared history for {} @ {} (quota: {}%)",
                                model.name, account.email, model.percentage
                            ));
                        }
                    }
                }
            }

            // 执行预热任务
            if !warmup_tasks.is_empty() {
                let total = warmup_tasks.len();
                logger::log_info(&format!(
                    "[Scheduler] 🔥 Triggering {} warmup tasks...",
                    total
                ));

                tokio::spawn(async move {
                    let mut success = 0;
                    for (idx, (email, model, token, pid, pct)) in warmup_tasks.into_iter().enumerate() {
                        logger::log_info(&format!(
                            "[Warmup {}/{}] {} @ {} ({}%)",
                            idx + 1, total, model, email, pct
                        ));

                        if quota::warmup_model_directly(&token, &model, &pid, &email, pct).await {
                            success += 1;
                        }

                        // 间隔 2 秒，避免请求过快
                        if idx < total - 1 {
                            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                        }
                    }

                    logger::log_info(&format!(
                        "[Scheduler] ✅ Warmup completed: {}/{} successful",
                        success, total
                    ));

                    // 刷新配额，同步到前端
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    let _ = crate::commands::refresh_all_quotas().await;
                });
            }

            // 扫描完成后刷新前端显示（确保调度器获取的最新数据同步到 UI）
            tokio::spawn(async {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                let _ = crate::commands::refresh_all_quotas().await;
                logger::log_info("[Scheduler] Quota data synced to frontend");
            });

            // 定期清理历史记录（保留最近 24 小时）
            {
                let now_ts = Utc::now().timestamp();
                let mut history = WARMUP_HISTORY.lock().unwrap();
                let cutoff = now_ts - 86400; // 24 小时前
                history.retain(|_, &mut ts| ts > cutoff);
            }
        }
    });
}
