//! Scenarios U5–U10: the semantics users rely on, checked
//! through real mounts. Each test starts its own lab and skips until the binaries are built.

mod support;
mod u10_history;
mod u5_concurrent_saves;
mod u6_team_share;
mod u7_offline_writes;
mod u8_fork;
mod u9_agents;
