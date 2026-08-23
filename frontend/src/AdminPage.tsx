import { LoaderCircle, Plus, Trash2, X } from "lucide-react";
import {
  type FormEvent,
  useCallback,
  useEffect,
  useState,
} from "react";

import {
  ApiRequestError,
  createAdminProvider,
  deleteAdminProvider,
  getAdminProviders,
  getAdminSession,
  loginAdmin,
  logoutAdmin,
  putDeepSeekProvider,
  setupAdmin,
  updateAdminProvider,
} from "./api";
import type {
  AdminModel,
  AdminProvider,
  AdminProviderWrite,
  AdminSession,
} from "./types";

const DEEPSEEK_MODELS: AdminModel[] = [
  { id: "deepseek-v4-flash", name: "DeepSeek V4 Flash", isDefault: true },
  { id: "deepseek-v4-pro", name: "DeepSeek V4 Pro", isDefault: false },
];

interface ProviderEditorState extends AdminProvider {
  exists: boolean;
  apiKey: string;
  clearApiKey: boolean;
}

function errorMessage(error: unknown, fallback: string): string {
  return error instanceof Error && error.message ? error.message : fallback;
}

function normalizeProvider(provider: AdminProvider): AdminProvider {
  const legacy = provider as AdminProvider & { apiKeyConfigured?: boolean };
  return {
    ...provider,
    baseUrl: provider.baseUrl ?? "",
    hasApiKey: provider.hasApiKey ?? Boolean(legacy.apiKeyConfigured),
    models: (provider.models ?? []).map((model) => ({ ...model })),
  };
}

function editorFromProvider(provider: AdminProvider): ProviderEditorState {
  const normalized = normalizeProvider(provider);
  return {
    ...normalized,
    exists: true,
    apiKey: "",
    clearApiKey: false,
  };
}

function newDeepSeekEditor(): ProviderEditorState {
  return {
    id: "deepseek",
    kind: "deepseek",
    name: "DeepSeek",
    baseUrl: "https://api.deepseek.com",
    enabled: false,
    hasApiKey: false,
    revision: 0,
    models: DEEPSEEK_MODELS.map((model) => ({ ...model })),
    exists: false,
    apiKey: "",
    clearApiKey: false,
  };
}

function newGenericEditor(): ProviderEditorState {
  return {
    id: "",
    kind: "openai_compatible",
    name: "",
    baseUrl: "",
    enabled: false,
    hasApiKey: false,
    revision: 0,
    models: [{ id: "", name: "", isDefault: true }],
    exists: false,
    apiKey: "",
    clearApiKey: false,
  };
}

function validateProvider(editor: ProviderEditorState): string | null {
  const name = editor.name.trim();
  const baseUrl = editor.baseUrl.trim();
  if (!name) return "请填写渠道名称。";
  if (name.length > 64 || /[\u0000-\u001f\u007f]/.test(name)) {
    return "渠道名称不能超过 64 个字符。";
  }
  if (!baseUrl) return "请填写 API Base URL。";
  if (baseUrl.length > 2048) return "API Base URL 过长。";
  try {
    const url = new URL(baseUrl);
    if (url.protocol !== "https:") {
      return "API Base URL 必须使用 https。";
    }
    if (url.username || url.password || url.search || url.hash) {
      return "API Base URL 不能包含账号、查询参数或片段。";
    }
  } catch {
    return "API Base URL 格式不正确。";
  }
  if (!editor.models.length) return "请至少添加一个模型。";
  if (editor.models.length > 20) return "每个渠道最多配置 20 个模型。";
  if (editor.models.some((model) => !model.id.trim() || !model.name.trim())) {
    return "每个模型都需要填写模型 ID 和显示名称。";
  }
  if (
    editor.models.some(
      (model) =>
        model.id.trim().length > 128 ||
        model.name.trim().length > 64 ||
        /[\u0000-\u001f\u007f]/.test(model.id) ||
        /[\u0000-\u001f\u007f]/.test(model.name),
    )
  ) {
    return "模型 ID 最多 128 个字符，显示名称最多 64 个字符。";
  }
  const ids = editor.models.map((model) => model.id.trim());
  if (new Set(ids).size !== ids.length) return "模型 ID 不能重复。";
  if (editor.models.filter((model) => model.isDefault).length !== 1) {
    return "请选择一个默认模型。";
  }
  const willHaveApiKey = editor.apiKey.trim()
    ? true
    : editor.clearApiKey
      ? false
      : editor.hasApiKey;
  if (editor.enabled && !willHaveApiKey) {
    return "启用渠道前请先填写 API 密钥。";
  }
  return null;
}

interface AuthPageProps {
  session: AdminSession;
  notice: string | null;
  onNotice: (notice: string | null) => void;
  onAuthenticated: (session: AdminSession) => void;
  onSessionChanged: (session: AdminSession) => void;
}

function AuthPage({
  session,
  notice,
  onNotice,
  onAuthenticated,
  onSessionChanged,
}: AuthPageProps) {
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [confirmPassword, setConfirmPassword] = useState("");
  const [setupToken, setSetupToken] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setError(null);
    const normalizedUsername = username.trim();
    if (!/^[A-Za-z0-9._-]{3,32}$/.test(normalizedUsername)) {
      setError("用户名需为 3–32 位字母、数字、点、下划线或短横线。");
      return;
    }
    const passwordCharacters = Array.from(password).length;
    if (passwordCharacters < 12 || passwordCharacters > 128) {
      setError("密码长度需为 12–128 个字符。");
      return;
    }
    if (session.setupRequired && password !== confirmPassword) {
      setError("两次输入的密码不一致。");
      return;
    }
    if (session.setupRequired && session.setupTokenRequired && !setupToken.trim()) {
      setError("请填写安装码。");
      return;
    }

    setSubmitting(true);
    try {
      const nextSession = session.setupRequired
        ? await setupAdmin({
            ...(session.setupTokenRequired
              ? { setupToken: setupToken.trim() }
              : {}),
            username: normalizedUsername,
            password,
          })
        : await loginAdmin({ username: normalizedUsername, password });
      setPassword("");
      setConfirmPassword("");
      setSetupToken("");
      onSessionChanged(nextSession);
      if (nextSession.authenticated) onAuthenticated(nextSession);
    } catch (submitError) {
      setPassword("");
      setConfirmPassword("");
      setSetupToken("");
      if (submitError instanceof ApiRequestError && submitError.status === 409) {
        try {
          const nextSession = await getAdminSession();
          onNotice("管理员已完成初始化，请登录。");
          onSessionChanged(nextSession);
        } catch {
          setError("管理员已完成初始化，请刷新后登录。");
        }
      } else {
        setError(
          errorMessage(
            submitError,
            session.setupRequired ? "注册失败，请重试。" : "登录失败，请重试。",
          ),
        );
      }
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <main id="main-content" className="auth-page">
      <section className="panel auth-panel" aria-labelledby="auth-title">
        <h1 id="auth-title">
          {session.setupRequired ? "创建首位管理员" : "管理员登录"}
        </h1>
        <p>
          {session.setupRequired
            ? "当前系统尚无管理员。完成注册后即可配置翻译渠道。"
            : "登录后可管理 DeepSeek 和 OpenAI 兼容渠道。"}
        </p>
        {notice && <div className="alert alert-warning" role="status">{notice}</div>}
        {error && <div className="alert alert-error" role="alert">{error}</div>}
        <form className="stack-form" onSubmit={submit} noValidate>
          {session.setupRequired && session.setupTokenRequired && (
            <label className="field">
              <span>安装码</span>
              <input
                type="password"
                aria-label="安装码"
                aria-describedby="setup-token-help"
                value={setupToken}
                maxLength={256}
                autoComplete="one-time-code"
                required
                onChange={(event) => setSetupToken(event.target.value)}
              />
              <small id="setup-token-help">安装码由服务器部署者提供，仅首次注册需要。</small>
            </label>
          )}
          <label className="field">
            <span>用户名</span>
            <input
              value={username}
              minLength={3}
              maxLength={32}
              pattern="[A-Za-z0-9._-]+"
              autoComplete="username"
              required
              onChange={(event) => setUsername(event.target.value)}
            />
          </label>
          <label className="field">
            <span>密码</span>
            <input
              type="password"
              aria-label="密码"
              aria-describedby={session.setupRequired ? "new-password-help" : undefined}
              value={password}
              minLength={12}
              maxLength={128}
              autoComplete={session.setupRequired ? "new-password" : "current-password"}
              required
              onChange={(event) => setPassword(event.target.value)}
            />
            {session.setupRequired && <small id="new-password-help">请使用至少 12 个字符的密码。</small>}
          </label>
          {session.setupRequired && (
            <label className="field">
              <span>确认密码</span>
              <input
                type="password"
                value={confirmPassword}
                minLength={12}
                maxLength={128}
                autoComplete="new-password"
                required
                onChange={(event) => setConfirmPassword(event.target.value)}
              />
            </label>
          )}
          <button className="button button-primary" type="submit" disabled={submitting}>
            {submitting && <LoaderCircle className="spin" aria-hidden="true" />}
            {submitting
              ? "正在提交"
              : session.setupRequired
                ? "创建管理员"
                : "登录"}
          </button>
        </form>
      </section>
    </main>
  );
}

interface ProviderFormProps {
  editor: ProviderEditorState;
  saving: boolean;
  error: string | null;
  onChange: (editor: ProviderEditorState) => void;
  onSave: () => void;
  onDelete: () => void;
  onClose: () => void;
}

function ProviderForm({
  editor,
  saving,
  error,
  onChange,
  onSave,
  onDelete,
  onClose,
}: ProviderFormProps) {
  const updateModel = (index: number, patch: Partial<AdminModel>) => {
    onChange({
      ...editor,
      models: editor.models.map((model, modelIndex) =>
        modelIndex === index ? { ...model, ...patch } : model,
      ),
    });
  };

  const makeDefault = (index: number) => {
    onChange({
      ...editor,
      models: editor.models.map((model, modelIndex) => ({
        ...model,
        isDefault: modelIndex === index,
      })),
    });
  };

  const removeModel = (index: number) => {
    const removedWasDefault = editor.models[index]?.isDefault;
    const models = editor.models.filter((_, modelIndex) => modelIndex !== index);
    if (removedWasDefault && models.length) models[0] = { ...models[0], isDefault: true };
    onChange({ ...editor, models });
  };

  return (
    <section className="panel provider-editor" aria-labelledby="provider-editor-title">
      <div className="panel-heading">
        <div>
          <h2 id="provider-editor-title">
            {editor.kind === "deepseek"
              ? "DeepSeek 配置"
              : editor.exists
                ? "编辑兼容渠道"
                : "添加兼容渠道"}
          </h2>
          <p>API 密钥只会提交到服务器，本页面不会保存或回显。</p>
        </div>
        <button className="icon-button" type="button" aria-label="关闭编辑器" onClick={onClose}>
          <X aria-hidden="true" />
        </button>
      </div>

      {error && <div className="alert alert-error" role="alert">{error}</div>}

      <form
        className="stack-form"
        noValidate
        onSubmit={(event) => {
          event.preventDefault();
          onSave();
        }}
      >
        <label className="field">
          <span>渠道名称</span>
          <input
            value={editor.name}
            maxLength={64}
            readOnly={editor.kind === "deepseek"}
            aria-readonly={editor.kind === "deepseek"}
            onChange={(event) => onChange({ ...editor, name: event.target.value })}
          />
        </label>
        <label className="field">
          <span>API Base URL</span>
          <input
            type="url"
            aria-label="API Base URL"
            aria-describedby={editor.kind === "deepseek" ? "deepseek-base-url-help" : undefined}
            value={editor.baseUrl}
            maxLength={2048}
            readOnly={editor.kind === "deepseek"}
            aria-readonly={editor.kind === "deepseek"}
            onChange={(event) => onChange({ ...editor, baseUrl: event.target.value })}
          />
          {editor.kind === "deepseek" && <small id="deepseek-base-url-help">DeepSeek 使用官方固定地址。</small>}
        </label>
        <label className="field">
          <span>API 密钥</span>
          <input
            type="password"
            aria-label="API 密钥"
            aria-describedby="provider-api-key-help"
            value={editor.apiKey}
            maxLength={4096}
            autoComplete="off"
            placeholder={editor.hasApiKey ? "留空以保留现有密钥" : "填写服务器端 API 密钥"}
            onChange={(event) =>
              onChange({ ...editor, apiKey: event.target.value, clearApiKey: false })
            }
          />
          <small id="provider-api-key-help">{editor.hasApiKey ? "服务器已配置密钥，密钥内容不会回显。" : "尚未配置密钥。"}</small>
        </label>
        {editor.hasApiKey && (
          <label className="checkbox-line">
            <input
              type="checkbox"
              checked={editor.clearApiKey}
              onChange={(event) =>
                onChange({
                  ...editor,
                  clearApiKey: event.target.checked,
                  apiKey: "",
                  enabled: event.target.checked ? false : editor.enabled,
                })
              }
            />
            清除服务器上的现有密钥
          </label>
        )}

        <div className="model-editor">
          <div className="subsection-heading">
            <div>
              <h3>模型</h3>
              <p>
                {editor.kind === "deepseek"
                  ? "DeepSeek 模型由官方接口固定提供。"
                  : "模型 ID 必须与上游 API 接受的 model 值一致。"}
              </p>
            </div>
            {editor.kind === "openai_compatible" && (
              <button
                className="button button-secondary button-small"
                type="button"
                disabled={editor.models.length >= 20}
                onClick={() =>
                  onChange({
                    ...editor,
                    models: [
                      ...editor.models,
                      { id: "", name: "", isDefault: editor.models.length === 0 },
                    ],
                  })
                }
              >
                <Plus aria-hidden="true" /> 添加模型
              </button>
            )}
          </div>

          <div className="model-list">
            {editor.models.map((model, index) => (
              <fieldset className="model-row" key={index}>
                <legend>模型 {index + 1}</legend>
                <label className="field">
                  <span>模型 ID</span>
                  <input
                    value={model.id}
                    maxLength={128}
                    readOnly={editor.kind === "deepseek"}
                    aria-readonly={editor.kind === "deepseek"}
                    onChange={(event) => updateModel(index, { id: event.target.value })}
                  />
                </label>
                <label className="field">
                  <span>显示名称</span>
                  <input
                    value={model.name}
                    maxLength={64}
                    readOnly={editor.kind === "deepseek"}
                    aria-readonly={editor.kind === "deepseek"}
                    onChange={(event) => updateModel(index, { name: event.target.value })}
                  />
                </label>
                <label className="radio-line">
                  <input
                    type="radio"
                    name="default-model"
                    checked={model.isDefault}
                    disabled={editor.kind === "deepseek"}
                    onChange={() => makeDefault(index)}
                  />
                  默认
                </label>
                {editor.kind === "openai_compatible" && (
                  <button
                    className="icon-button icon-button-danger"
                    type="button"
                    aria-label={`删除模型 ${model.name || index + 1}`}
                    onClick={() => removeModel(index)}
                  >
                    <Trash2 aria-hidden="true" />
                  </button>
                )}
              </fieldset>
            ))}
            {!editor.models.length && <p className="empty-list">尚未添加模型。</p>}
          </div>
        </div>

        <label className="checkbox-line">
          <input
            type="checkbox"
            checked={editor.enabled}
            onChange={(event) => onChange({ ...editor, enabled: event.target.checked })}
          />
          启用此渠道供管理员翻译使用
        </label>
        {editor.kind !== "google" && (
          <p className="form-note">
            此渠道仅对已登录管理员可见。翻译进程在运行时会使用 API 密钥，请勿上传来源不可信的 PDF。
          </p>
        )}

        <div className="form-actions">
          <button className="button button-primary" type="submit" disabled={saving}>
            {saving && <LoaderCircle className="spin" aria-hidden="true" />}
            {saving ? "正在保存" : "保存配置"}
          </button>
          {editor.exists && editor.kind === "openai_compatible" && (
            <button className="button button-danger" type="button" onClick={onDelete} disabled={saving}>
              <Trash2 aria-hidden="true" /> 删除渠道
            </button>
          )}
        </div>
      </form>
    </section>
  );
}

interface DashboardProps {
  session: AdminSession;
  onExpired: (message: string) => void;
  onLogout: () => void;
}

function Dashboard({ session, onExpired, onLogout }: DashboardProps) {
  const [providers, setProviders] = useState<AdminProvider[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [editor, setEditor] = useState<ProviderEditorState | null>(null);
  const [editorError, setEditorError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const csrfToken = session.csrfToken ?? "";

  const expireIfNeeded = useCallback((error: unknown): boolean => {
    if (error instanceof ApiRequestError && error.status === 401) {
      setEditor((current) => current ? { ...current, apiKey: "" } : null);
      onExpired("登录已过期，请重新登录。");
      return true;
    }
    return false;
  }, [onExpired]);

  const loadProviders = useCallback(async () => {
    setLoading(true);
    setLoadError(null);
    try {
      const result = (await getAdminProviders()).map(normalizeProvider);
      setProviders(result);
    } catch (error) {
      if (!expireIfNeeded(error)) {
        setLoadError(errorMessage(error, "无法读取渠道配置。"));
      }
    } finally {
      setLoading(false);
    }
  }, [expireIfNeeded]);

  useEffect(() => {
    void loadProviders();
  }, [loadProviders]);

  const openDeepSeek = () => {
    const provider = providers.find((item) => item.kind === "deepseek");
    setEditor(provider ? editorFromProvider(provider) : newDeepSeekEditor());
    setEditorError(null);
  };

  const saveProvider = async () => {
    if (!editor) return;
    const validationError = validateProvider(editor);
    setEditorError(validationError);
    if (validationError) return;

    const payload: AdminProviderWrite = {
      kind: editor.kind === "deepseek" ? "deepseek" : "openai_compatible",
      name: editor.name.trim(),
      baseUrl: editor.baseUrl.trim().replace(/\/$/, ""),
      enabled: editor.enabled,
      ...(editor.apiKey.trim() ? { apiKey: editor.apiKey.trim() } : {}),
      clearApiKey: editor.clearApiKey,
      ...(editor.exists ? { expectedRevision: editor.revision } : {}),
      models: editor.models.map((model) => ({
        id: model.id.trim(),
        name: model.name.trim(),
        isDefault: model.isDefault,
      })),
    };

    // Clear the write-only secret before the request completes, including on 401.
    setEditor({ ...editor, apiKey: "" });
    setSaving(true);
    setEditorError(null);
    try {
      if (editor.kind === "deepseek") {
        await putDeepSeekProvider(payload, csrfToken);
      } else if (editor.exists) {
        await updateAdminProvider(editor.id, payload, csrfToken);
      } else {
        await createAdminProvider(payload, csrfToken);
      }
      await loadProviders();
      setEditor(null);
    } catch (error) {
      if (!expireIfNeeded(error)) {
        setEditorError(errorMessage(error, "保存失败，请重试。"));
      }
    } finally {
      setSaving(false);
    }
  };

  const removeProvider = async () => {
    if (!editor || !editor.exists || editor.kind !== "openai_compatible") return;
    if (!window.confirm(`确定删除渠道“${editor.name}”吗？`)) return;
    setEditor({ ...editor, apiKey: "" });
    setSaving(true);
    setEditorError(null);
    try {
      await deleteAdminProvider(editor.id, editor.revision, csrfToken);
      await loadProviders();
      setEditor(null);
    } catch (error) {
      if (!expireIfNeeded(error)) {
        setEditorError(errorMessage(error, "删除失败，请重试。"));
      }
    } finally {
      setSaving(false);
    }
  };

  return (
    <>
      <header className="site-header">
        <div className="site-header-inner">
          <a className="site-title" href="/admin">管理后台</a>
          <nav aria-label="管理导航">
            <a href="/">返回翻译</a>
            <span className="admin-username">{session.username}</span>
            <button className="link-button" type="button" onClick={onLogout}>退出登录</button>
          </nav>
        </div>
      </header>
      <main id="main-content" className="admin-content">
        <div className="page-intro">
          <div>
            <h1>翻译渠道</h1>
            <p>在服务器端维护 API 密钥和可选模型。</p>
          </div>
          <button className="button button-primary" type="button" onClick={() => {
            setEditor(newGenericEditor());
            setEditorError(null);
          }}>
            <Plus aria-hidden="true" /> 添加兼容渠道
          </button>
        </div>

        {loadError && (
          <div className="alert alert-error" role="alert">
            <span>{loadError}</span>
            <button className="link-button" type="button" onClick={() => void loadProviders()}>
              重新加载
            </button>
          </div>
        )}

        <div className="admin-layout">
          <section className="panel provider-list-panel" aria-labelledby="provider-list-title">
            <div className="panel-heading">
              <h2 id="provider-list-title">已配置渠道</h2>
              {loading && <LoaderCircle className="spin" aria-label="正在加载" />}
            </div>
            <div className="provider-list">
              <div className="provider-list-item">
                <div>
                  <strong>Google</strong>
                  <span>系统内置渠道</span>
                </div>
                <span className="status status-succeeded">内置</span>
              </div>
              {providers
                .filter((provider) => provider.kind !== "google")
                .map((provider) => (
                  <button
                    className="provider-list-item provider-list-button"
                    type="button"
                    key={provider.id}
                    onClick={() => {
                      setEditor(editorFromProvider(provider));
                      setEditorError(null);
                    }}
                  >
                    <span>
                      <strong>{provider.name}</strong>
                      <small>
                        {provider.kind === "deepseek" ? "DeepSeek" : "OpenAI 兼容"}
                        {` · ${provider.models.length} 个模型`}
                      </small>
                    </span>
                    <span className={`status ${provider.enabled ? "status-succeeded" : "status-cancelled"}`}>
                      {provider.enabled ? "已启用" : "未启用"}
                    </span>
                  </button>
                ))}
              {!providers.some((provider) => provider.kind === "deepseek") && (
                <button className="provider-list-item provider-list-button" type="button" onClick={openDeepSeek}>
                  <span>
                    <strong>DeepSeek</strong>
                    <small>尚未配置</small>
                  </span>
                  <span className="status status-cancelled">未配置</span>
                </button>
              )}
            </div>
            <button className="button button-secondary configure-deepseek" type="button" onClick={openDeepSeek}>
              配置 DeepSeek
            </button>
          </section>

          {editor ? (
            <ProviderForm
              editor={editor}
              saving={saving}
              error={editorError}
              onChange={setEditor}
              onSave={() => void saveProvider()}
              onDelete={() => void removeProvider()}
              onClose={() => {
                setEditor(null);
                setEditorError(null);
              }}
            />
          ) : (
            <section className="panel admin-empty" aria-label="渠道编辑区">
              <p>从左侧选择渠道进行编辑，或添加新的 OpenAI 兼容渠道。</p>
            </section>
          )}
        </div>
      </main>
    </>
  );
}

export default function AdminPage() {
  const [session, setSession] = useState<AdminSession | null>(null);
  const [loading, setLoading] = useState(true);
  const [notice, setNotice] = useState<string | null>(null);
  const [loggingOut, setLoggingOut] = useState(false);

  useEffect(() => {
    let disposed = false;
    void getAdminSession()
      .then((result) => {
        if (!disposed) setSession(result);
      })
      .catch((error) => {
        if (!disposed) setNotice(errorMessage(error, "无法读取管理员会话。"));
      })
      .finally(() => {
        if (!disposed) setLoading(false);
      });
    return () => {
      disposed = true;
    };
  }, []);

  const expireSession = (message: string) => {
    setNotice(message);
    setSession((current) => ({
      setupRequired: current?.setupRequired ?? false,
      setupTokenRequired: current?.setupTokenRequired ?? false,
      authenticated: false,
    }));
  };

  const logout = async () => {
    if (!session?.csrfToken) return;
    setLoggingOut(true);
    setNotice(null);
    try {
      await logoutAdmin(session.csrfToken);
      setSession({
        setupRequired: false,
        setupTokenRequired: false,
        authenticated: false,
      });
    } catch (error) {
      if (error instanceof ApiRequestError && error.status === 401) {
        expireSession("登录已过期，请重新登录。");
      } else {
        setNotice(errorMessage(error, "退出失败，请重试。"));
      }
    } finally {
      setLoggingOut(false);
    }
  };

  if (loading) {
    return (
      <div className="app-page">
        <main className="auth-page" role="status">
          <LoaderCircle className="spin loading-large" aria-hidden="true" />
          <p>正在读取管理员状态…</p>
        </main>
      </div>
    );
  }

  if (!session) {
    return (
      <div className="app-page">
        <main className="auth-page">
          <section className="panel auth-panel">
            <h1>管理后台不可用</h1>
            <div className="alert alert-error" role="alert">{notice}</div>
            <button className="button button-secondary" type="button" onClick={() => window.location.reload()}>
              重新加载
            </button>
          </section>
        </main>
      </div>
    );
  }

  return (
    <div className="app-page">
      <a className="skip-link" href="#main-content">跳到主要内容</a>
      {session.authenticated ? (
        <Dashboard
          session={session}
          onExpired={expireSession}
          onLogout={() => {
            if (!loggingOut) void logout();
          }}
        />
      ) : (
        <>
          <header className="site-header">
            <div className="site-header-inner">
              <a className="site-title" href="/admin">管理后台</a>
              <nav aria-label="管理导航"><a href="/">返回翻译</a></nav>
            </div>
          </header>
          <AuthPage
            key={session.setupRequired ? "setup" : "login"}
            session={session}
            notice={notice}
            onNotice={setNotice}
            onAuthenticated={(nextSession) => {
              setNotice(null);
              setSession(nextSession);
            }}
            onSessionChanged={setSession}
          />
        </>
      )}
    </div>
  );
}
