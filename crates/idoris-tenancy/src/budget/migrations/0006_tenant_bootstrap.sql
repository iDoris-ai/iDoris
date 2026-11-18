-- S1 migration 0006: 租户来源以数据库为准（设计稿 §9 第 2 项，组织 / 云形态）。
--
-- 配置文件里的租户只用来做**一次性初始化**：每个租户 id 在这个库的一生里最多从配置文件写入一次，
-- 写入的同时记一行在这里。之后的启动看到这行，就不会再碰 `tenant_config`：
--   * 不会拿配置文件的值覆盖数据库里已经改过的限额；
--   * 不会把被管理员有意删掉的租户「复活」；
--   * 配置文件里没有这个租户，也不会删它（组织 / 云形态不再调用 `retain_tenants`）。
--
-- 故意不加指向 `tenant_config` 的外键：标记必须比配置行活得久，否则「被删的租户」会被下次启动重新初始化。
--
-- 纯加法：不动 0001–0005 建出来的任何表和任何一行数据。个人形态和存量 tenant 形态从不读写这张表。
--
-- source:
--   'config'  = 这次是从配置文件新写入的；
--   'adopted' = 库里早就有这个租户的配置（旧版本启动写下的），原样保留，只补上标记。
CREATE TABLE IF NOT EXISTS tenant_bootstrap (
    tenant_id    TEXT NOT NULL PRIMARY KEY,
    seeded_at_ms INTEGER NOT NULL,
    source       TEXT NOT NULL CHECK (source IN ('config', 'adopted'))
) STRICT;
