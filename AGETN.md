# 飞梭 (Feisuo) · 开发规范与协作准则 (AGETN.md)

> 本文件与 [AGENTS.md](file:///d:/dev/agent-repos/feisuo/AGENTS.md) 保持完全一致，作为开发规范索引。详细内容请参阅 [AGENTS.md](file:///d:/dev/agent-repos/feisuo/AGENTS.md)。

---

## 核心红线准则速查
1. **Git 提交纪律**：未经用户明确要求，严禁擅自执行 `git commit` 或 `git push`！避免产生过多、过杂的 git 提交记录。
2. **测试红线**：禁止运行全量测试或破坏性测试。
3. **架构选型**：Tauri v2 + Rust Core + 响应式前端。
4. **开机自启与自连**：Windows 托盘守护 + Android 前台常驻保活服务 (Foreground Service)。
