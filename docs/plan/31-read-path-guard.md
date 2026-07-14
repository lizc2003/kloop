# Plan 31 — grep/glob 路径级保护(读类工具统一敏感过滤)

> 一句话定位:kloop 的路径级 deny(`read_file(**/*.pem)`)和敏感路径检查(`.ssh`/`.env`/
> `.git`/`.kloop`)只在 read/write/edit_file 的权限门跑;**grep/glob 完全绕过**——`grep`
> 读 `.env` 内容照样泄密、`read_file(**/*.pem)` deny 挡不住 `grep` 读同一文件、`glob`
> 照列敏感路径。补:把"读可及性"判定统一应用到读类工具(read_file / grep / glob)。

## 回源结论(2026-07-14)

- **claude-code —— 统一读忽略模式(主参考)**:**不**给 Grep/Glob 加 allow/deny 规则,
  而是 `getFileReadIgnorePatterns(appState.toolPermissionContext)` 生成一组"读忽略模式",
  在 GrepTool/GlobTool 扫描时**过滤输出**(`GrepTool.ts:411`;`GlobTool.ts:155`)。即
  deny 是"读忽略",读类工具统一尊重,不是每工具各写一条规则。
- **codex —— 无(反面)**:shell 万能导致"权限粒度粗"(`refs/README.md:15`),无专用
  Grep/Glob 的路径保护。
- **claw —— 无**。

**关键取向**:痛点原话(plan 14:53)是"用户写了 `read_file(**/*.pem)` deny,期望它挡住
所有读"——所以正解是**让已有的 read deny + 敏感路径对 grep/glob 统一生效**(cc 的
`getFileReadIgnorePatterns` 精神),而不是逼用户再写一条 `grep(**/*.pem)`。

## kloop 现状与落点(已读准,`crates/core/src/permissions.rs`)

- `Rule::PathGlob { tool, glob }`(:123-126)已支持 `read_file`/`write_file`/`edit_file`;
  `matches_path`(:149)按相对/规范化路径匹配 gitignore 风格 glob。
- `path_is_sensitive(&normalized)`(:587)+ `PathFacts.sensitive`(:509)——敏感路径
  (`.git`/`.ssh`/`.env*`/`.kloop`)判定,safety check **bypass 免疫**(:9)。
- **缺口**:grep/glob 工具(`tools/` 内,参数 `path`=搜索根、`glob`=文件过滤、
  `pattern`=正则)在执行时**不查** PathGlob deny、不查 `path_is_sensitive`——它们的 `path`
  是目录根,一次读根下一堆文件,权限门那套"单路径 ask"不适配。

**落点(对齐 cc,运行时输出过滤,非权限门 ask)**:
- 抽一个"读可及性"判定 `read_path_blocked(path, &Permissions) -> bool` = `path_is_sensitive`
  **或** 命中 read_file 的 deny PathGlob(把 read_file 的 deny 语义复用给读类工具)。
- grep/glob 在收集结果时对每个命中文件/路径跑该判定,**命中则从结果剔除**;末尾附一句
  `[N path(s) hidden by deny/sensitive rules]` 让模型知道有遮挡(不静默骗它"没有")。
- read_file 自身行为不变(它已在权限门被 deny/敏感拦);本 plan 只补 grep/glob 这两个"读
  一堆文件却没过滤"的口子。

## 关键决定(开工时定 / 问用户)

1. **剔除 vs 整调用拒绝**:grep 搜整树顺带碰到一个 `.pem`——静默跳过该文件 +
   计数提示(cc 式,倾向)还是整个 grep 调用报错?倾向**跳过 + 提示**(搜索仍有用,只是
   遮蔽敏感命中)。
2. **deny 来源**:复用 read_file 的 deny PathGlob(用户一条 `read_file(**/*.pem)` 对读类
   统一生效,cc 精神,**倾向**)vs 新增独立 `grep(<glob>)`/`glob(<glob>)` 规则(plan 14
   备忘的形)。倾向前者;后者作挂账(真需要"grep 可搜但 read 不可读"这类非对称时再加)。
3. **敏感路径纳入**:`path_is_sensitive`(`.env`/`.ssh` 等)是否一并过滤 grep/glob 输
   出。**是**——这是主痛点(grep 读 `.env` 泄密)。

## 不做(挂账)

grep/glob 的独立 **allow** 规则(读类默认可读,只做 deny/敏感的减法过滤);独立
`grep(<glob>)` 规则形(决定 2 取"复用 read deny"时);grep `path` 搜索根本身的 ask 门
(粒度不对,不做);性能(命中集上跑 globset,量级同现有 gitignore 尊重,不优化);把过
滤延伸到 bash 里的 `cat`/`rg`(shell 是 Opaque,归 shell 安全检查那条线,不在本 plan)。

## 测试

建一棵含 `secret.pem`/`.env` 的临时树:①`grep(content)` 搜通配 + deny `read_file(**/*.pem)`
→ 结果无 `.pem` 命中 + 遮挡提示;②`grep` 搜 `.env` 内容 → 命中被过滤(敏感路径);
③`glob(**)` → 不列 `.pem`/`.env`;④无 deny/非敏感文件正常返回;⑤`read_path_blocked`
单元(sensitive 命中、deny glob 命中、普通路径放行)。

## 完成标准

fmt/clippy/test 全绿,一次 commit;README 同步"读类工具统一尊重 deny/敏感路径";本文件
补完成记录(提交号 + 挂账);HANDOFF 补教训(路径级保护从"权限门单路径 ask"扩到"读类工
具输出过滤"这个粒度转换)。真 key 非必需(纯本地判定,单测足够;可选验一次模型 grep 敏
感目录被遮挡后的表现)。
