//! idoris.ai/api 计算服务 S0：契约冻结。
pub mod account;
pub mod admission;
pub mod attempt;
pub mod credential;
pub mod grant;
pub mod ids;
pub mod money;
pub mod pricing;
pub mod reason;
pub mod request;
pub mod reserve;
#[cfg(test)]
pub(crate) mod test_support;
