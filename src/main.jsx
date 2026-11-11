import React, {useEffect, useState} from 'react';
import {createRoot} from 'react-dom/client';
import {invoke} from '@tauri-apps/api/core';
import {listen} from '@tauri-apps/api/event';
import {Panel} from './design-system/ui/layout';
import {Button, Input} from './design-system/ui/forms';
import {Badge} from './design-system/ui/data';
import {ConfirmDialog} from './design-system/ui/feedback';
import {DropdownMenu} from './design-system/ui/DropdownMenu';
import {Icon} from './design-system/ui/icons';
import {AtelierBrand} from './design-system/AtelierBrand';
import {Setup} from './views/Setup';
import freeMark from './assets/atelier-mark.svg';
import './design-system/tokens.css';
import './design-system/styles/base.css';
import './design-system/styles/primitives.css';
import './design-system/styles/shell.css';
import './style.css';

const INSTALL_NAME = 'Atelier Free';

// 桌面外壳,不是浏览器:屏蔽右键菜单与浏览器快捷键,避免露出 WebView 的身份。
document.addEventListener('contextmenu', event => event.preventDefault());
document.addEventListener('selectstart', event => { if (!event.target.closest('input,textarea')) event.preventDefault(); });

/** A capability row. Read-only: the launcher reports state, it does not drive the
 *  domain lifecycle. A paid capability that is not installed is the purchase entry. */
function DomainRow({domain, onUnlock}) {
  const locked = domain.paid && !domain.licensed;
  const state = !domain.installed
    ? '未安装'
    : domain.running == null ? '未运行' : domain.running ? '运行中' : '已停止';
  const tone = !domain.installed ? 'neutral' : domain.running ? 'success' : 'neutral';
  return <div className={`cap-row ${locked ? 'locked' : ''}`}>
    <span className="cap-icon"><Icon name={domain.icon} size={16}/></span>
    <div className="cap-copy">
      <strong>{domain.title}</strong>
      <small>{domain.description}</small>
    </div>
    <div className="cap-state">
      {locked
        ? <button type="button" className="cap-unlock" onClick={()=>onUnlock(domain)}>未授权 · 了解授权</button>
        : <Badge tone={tone}>{state}</Badge>}
    </div>
  </div>;
}

/** Full-card overlay while the installer runs. `percent: null` means "unknown": spinner, no number. */
const PHASE_TEXT = {
  resolve: '获取中', plan: '准备中', download: '下载中', verify: '校验中',
  stage: '解压中', swap: '换入中', env: '构建环境', start: '启动中',
  check: '检查中', done: '完成', fail: '失败',
};

function UpdateOverlay({progress, log}) {
  const pct = typeof progress.percent === 'number' ? Math.max(0, Math.min(100, progress.percent)) : null;
  const spinning = pct == null;
  const radius = 54;
  const circumference = 2 * Math.PI * radius;
  const short = PHASE_TEXT[progress.phase] || '处理中';
  return <div className="update-overlay" role="status" aria-live="polite">
    <div className={`update-ring ${spinning ? 'is-spinning' : ''}`}>
      <svg viewBox="0 0 120 120" width="120" height="120" aria-hidden="true">
        <circle className="ring-track" cx="60" cy="60" r={radius}/>
        <circle className="ring-value" cx="60" cy="60" r={radius}
          strokeDasharray={spinning ? `${circumference * 0.28} ${circumference * 0.72}` : circumference}
          strokeDashoffset={spinning ? 0 : circumference * (1 - pct / 100)}/>
      </svg>
      <span className={`update-percent ${spinning ? 'is-spinning' : ''}`}>{spinning ? short : `${Math.round(pct)}%`}</span>
      {typeof progress.rate === 'number' && progress.rate > 0 && <span className="update-rate">{`${(progress.rate / 1048576).toFixed(1)} MB/s`}</span>}
    </div>
    <p className="update-label">{progress.label}</p>
    <p className="update-detail">{progress.detail || ' '}</p>
    <p className="update-log">{log || ' '}</p>
  </div>;
}function App() {
  const [booting, setBooting] = useState(true);
  const [needsSetup, setNeedsSetup] = useState(false);
  const [cap, setCap] = useState(null);
  const [view, setView] = useState('capabilities');
  const [shared, setShared] = useState(null);
  const [tokenInput, setTokenInput] = useState('');
  const [savingShared, setSavingShared] = useState(false);
  const [sharedNote, setSharedNote] = useState('');
  const [workbenchPort, setWorkbenchPort] = useState(null);
  const [busy, setBusy] = useState('');
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const [confirm, setConfirm] = useState(null);
  const [progress, setProgress] = useState(null);
  const [updateLog, setUpdateLog] = useState('');

  // Installer progress: download / verify / stage / swap each emit one event per tick.
  useEffect(() => {
    let off;
    (async () => {
      off = await listen('atelier://progress', event => {
        const next = event.payload || {};
        setProgress(next);
        if (next.log) setUpdateLog(next.log);
        if (next.phase === 'done' || next.phase === 'fail') {
          setTimeout(() => { setProgress(null); setUpdateLog(''); }, 2500);
        }
      });
    })();
    return () => { off?.(); };
  }, []);

  async function refresh() {
    try {
      const [nextCap, nextShared] = await Promise.all([invoke('capabilities'), invoke('shared_settings')]);
      setCap(nextCap);
      setShared(nextShared);
    } catch (err) { setError(String(err)); }
  }

  useEffect(() => {
    let disposed = false;
    let unlisten;
    let unlistenSetup;
    (async () => {
      try {
        unlisten = await listen('variant-status', () => { if (!disposed) void refresh(); });
        unlistenSetup = await listen('atelier://setup', () => setNeedsSetup(true));
        if (disposed) { unlisten(); unlistenSetup(); return; }
        const state = await invoke('setup_state');
        if (disposed) return;
        setNeedsSetup(state.needsSetup);
        if (!state.needsSetup) await refresh();
      } catch (err) { if (!disposed) setError(String(err)); }
      finally { if (!disposed) setBooting(false); }
    })();
    const onFocus = () => { void refresh(); };
    window.addEventListener('focus', onFocus);
    return () => { disposed = true; unlisten?.(); unlistenSetup?.(); window.removeEventListener('focus', onFocus); };
  }, []);

  async function run(action, extra = {}) {
    setBusy(action); setMessage(''); setError('');
    try {
      const result = await invoke(action, extra);
      if (action === 'open_workbench') {
        setWorkbenchPort(result);
      } else if (action === 'stop_variant') {
        setWorkbenchPort(null);
        setMessage(result === false ? '已强制结束：当前 Kernel 没有优雅停机接口。' : '已停止。');
      } else if (action === 'install_variant') {
        // 更新即启动:成功时后端会把实例端口带回来,直接进工作台。
        const port = Number(String(result).match(/端口 (\d+)/)?.[1] || 0);
        if (port > 0) setWorkbenchPort(port);
        setMessage(String(result));
      } else if (action === 'uninstall_variant') {
        setWorkbenchPort(null);
        setMessage('已卸载程序，用户数据已保留。');
      }
      await refresh();
    } catch (err) { setError(String(err)); }
    finally { setBusy(''); }
  }

  async function toggle(domain, next) {
    setError(''); setMessage('');
    if (next) return run('set_domain', {domain: domain.id, enabled: true});
    setBusy(`toggle:${domain.id}`);
    try {
      // Ask first: a domain with work in progress must not be switched off silently.
      const activity = cap?.port
        ? await invoke('domain_activity', {port: cap.port, domain: domain.id})
        : {busy: 0, detail: ''};
      if (activity.needsConfirmation) {
        setConfirm({kind: 'toggle', domain, detail: activity.detail || '无法确认该域是否空闲。'});
      } else {
        await invoke('set_domain', {domain: domain.id, enabled: false});
        setMessage(`${domain.title} 已关闭。`);
        await refresh();
      }
    } catch (err) { setError(String(err)); }
    finally { setBusy(''); }
  }

  async function startInstall() {
    setError(''); setMessage('');
    try {
      const foreign = await invoke('foreign_installs');
      if (foreign.length) { setConfirm({kind: 'start', foreign}); return; }
      await run('open_workbench');
    } catch (err) { setError(String(err)); }
  }

  async function confirmAction() {
    const pending = confirm;
    setConfirm(null);
    if (!pending) return;
    if (pending.kind === 'launcher') {
      try {
        await invoke('apply_self_update', {tag: pending.tag});
        setMessage('启动器即将退出并完成更新…');
      } catch (err) { setError(String(err)); }
      return;
    }
    if (pending.kind === 'start') { await run('open_workbench', {stopForeign: true}); return; }
    if (pending.kind === 'toggle') {
      setBusy(`toggle:${pending.domain.id}`);
      try {
        await invoke('set_domain', {domain: pending.domain.id, enabled: false});
        setMessage(`${pending.domain.title} 已关闭。`);
        await refresh();
      } catch (err) { setError(String(err)); }
      finally { setBusy(''); }
      return;
    }
    await run(pending.action);
  }

  const installed = cap?.installed ?? false;
  const running = !!(cap?.port && cap.domains.some(domain => domain.running));

  async function chooseModels() {
    try {
      const picked = await invoke('choose_directory', {kind:'models', current: shared?.modelsDir || null});
      if (picked) setShared(current => ({...current, modelsDir: picked}));
    } catch (err) { setError(String(err)); }
  }

  async function saveShared() {
    setSavingShared(true); setSharedNote(''); setError('');
    try {
      const result = await invoke('save_shared', {
        modelsDir: shared?.modelsDir || '',
        ollamaHost: shared?.ollamaHost || '',
        token: tokenInput || null,
      });
      setSharedNote(result.note);
      setTokenInput('');
      await refresh();
    } catch (err) { setError(String(err)); }
    finally { setSavingShared(false); }
  }

  /** 启动器自身的更新:检查 → 确认 → 下载并退出换文件。 */
  async function checkLauncherUpdate() {
    try {
      const info = await invoke('check_self_update');
      if (!info.available) { setMessage(`启动器已是最新（v${info.current}）`); return; }
      setConfirm({kind: 'launcher', tag: info.latest, title: `更新启动器到 v${info.latest}`,
        body: '下载并校验完成后，启动器会退出并自动换上新版本，大约 5 秒后自动重启。', label: '更新启动器'});
    } catch (err) { setError(String(err)); }
  }
  const menuItems = [
    {value:'data', label:'打开数据目录', onSelect:()=>void invoke('open_runtime_path', {kind:'data'})},
    {value:'models', label:'打开模型目录', onSelect:()=>void invoke('open_runtime_path', {kind:'models'})},
    {value:'setup', label:'重新运行初始设置', onSelect:()=>void invoke('reopen_setup')},
    {value:'update', label:'更新发行包', onSelect:()=>setConfirm({kind:'action', action:'install_variant', title:'更新 Atelier Free', body:'更新会停止当前实例，运行中的任务可能中断。', label:'更新'})},
    {value:'launcher', label:'更新启动器', onSelect:()=>void checkLauncherUpdate()},
    {value:'uninstall', label:'卸载', danger:true, onSelect:()=>setConfirm({kind:'action', action:'uninstall_variant', title:'卸载 Atelier Free', body:'卸载程序后，数据、模型和引擎环境将保留。', label:'卸载'})},
  ];

  if (booting) {
    return <div className="boot" role="status" aria-label="正在启动 Atelier">
      <img className="boot-mark" src={freeMark} alt=""/>
      <div className="boot-word">atelier<small>CREATIVE SYSTEM</small></div>
      <div className="boot-bar"><span/></div>
    </div>;
  }
  if (needsSetup) return <Setup onDone={async () => { setNeedsSetup(false); await refresh(); }}/>;

  return <>
    {!workbenchPort && <div className="launcher">
      <header className="launcher-header"><AtelierBrand/><div className="header-actions">
        <button type="button" className={`header-icon ${view === 'settings' ? 'active' : ''}`} aria-label="全局设置" title="全局设置" onClick={()=>setView(view === 'settings' ? 'capabilities' : 'settings')}><Icon name="sliders" size={16}/></button>
        <div className="header-context"><span className="tiny-dot"/>桌面工作台</div>
      </div></header>
      <main className="launcher-main">
        <div className="page-intro"><div><div className="eyebrow">ATELIER / CAPABILITIES</div><h1>域能力</h1><p>每一项是一个能力单元；这里显示它是否安装、当前是否在这个实例里运行。</p></div></div>

        {view === 'capabilities' && <Panel className="instance-card">
          {progress && <UpdateOverlay progress={progress} log={updateLog}/>}
          <div className="instance-head">
            <img className="variant-mark" src={freeMark} alt=""/>
            <div className="instance-title">
              <h2>{INSTALL_NAME}</h2>
              <span className="card-kind">{cap?.version ? `v${cap.version}` : '版本未知'}{installed ? ` · ${cap?.installDir}` : ' · 尚未安装'}</span>
            </div>
            <div className="instance-state">
              <Badge tone={running ? 'success' : 'neutral'}>{!installed ? '未安装' : running ? '运行中' : '已停止'}</Badge>
              {cap?.port && <span className="cap-hint">127.0.0.1:{cap.port}</span>}
            </div>
            <div className="instance-actions">
              {installed
                ? running
                  ? <Button size="small" onClick={()=>setConfirm({kind:'action', action:'stop_variant', title:'停止 Atelier Free', body:'停止会中断正在运行的任务。Kernel 会关闭并回收它管理的域。', label:'停止'})} disabled={!!busy}>停止</Button>
                  : <Button size="small" variant="primary" onClick={startInstall} disabled={!!busy}>{busy === 'open_workbench' ? '启动中…' : '启动'}</Button>
                : <Button size="small" variant="primary" onClick={()=>run('install_variant')} disabled={!!busy}>安装</Button>}
              {installed && running && <Button size="small" variant="primary" onClick={()=>run('open_workbench')} disabled={!!busy}>打开工作台</Button>}
              <DropdownMenu className="card-menu" label={`${INSTALL_NAME} 操作`} title={`${INSTALL_NAME} 操作`} disabled={!!busy} items={menuItems}>⋯</DropdownMenu>
            </div>
          </div>

          <div className="cap-section">
            <div className="cap-section-head"><h3>能力</h3><span>{cap ? `${cap.domains.filter(domain=>domain.installed).length} 个已安装` : '读取中'}</span></div>
            {cap
              ? cap.domains.map(domain => <React.Fragment key={domain.id}>
                  <DomainRow domain={domain} onUnlock={locked => setMessage(`${locked.title} 需要单独授权；授权流程尚未接入。`)}/>
                  {domain.id === 'pipelines' && domain.installed && cap.pipelines.length > 0 && <div className="cap-sublist">
                    {cap.pipelines.map(pipeline => <div className="cap-subrow" key={pipeline.id}>
                      <span className="cap-sub-icon"><Icon name="layers" size={13}/></span>
                      <strong>{pipeline.label}</strong>
                      <small>{pipeline.id}</small>
                      <Badge tone="success">可用</Badge>
                    </div>)}
                  </div>}
                </React.Fragment>)
              : <p className="empty-group">正在读取能力清单…</p>}
          </div>
        </Panel>}

        {view === 'settings' && <Panel className="instance-card">
          <div className="cap-section">
            <div className="cap-section-head"><h3>全局设置</h3><span>所有安装共用</span></div>
            <div className="setting-bar">
              <span className="setting-label">权重目录</span>
              <div className="setting-path">{shared?.modelsDir || shared?.modelsDirEffective || '安装目录默认位置'}</div>
              <Button size="small" onClick={chooseModels} disabled={savingShared}>选择目录</Button>
            </div>
            <div className="setting-bar">
              <span className="setting-label">LLM 服务</span>
              <Input className="setting-input" value={shared?.ollamaHost || ''} placeholder="http://127.0.0.1:11434"
                onChange={event=>setShared(current=>({...current, ollamaHost:event.target.value}))}/>
            </div>
            <div className="setting-bar">
              <span className="setting-label">HF Token</span>
              <Input className="setting-input" type="password" value={tokenInput} autoComplete="off" spellCheck={false}
                placeholder={shared?.tokenHint ? `已保存 ${shared.tokenHint}` : '未设置'}
                onChange={event=>setTokenInput(event.target.value)}/>
              <Button size="small" variant="primary" onClick={saveShared} disabled={savingShared}>{savingShared ? '保存中…' : '保存'}</Button>
            </div>
            <p className="setting-note">{sharedNote || (shared?.live
              ? '实例运行中：保存后立即同步到引擎设置，下一次调用生效，不会中断正在执行的任务。'
              : '实例未运行：保存后下次启动生效。')}</p>
          </div>
        </Panel>}

        {busy.startsWith('toggle:') && <p className="operation-message" role="status">正在切换能力…</p>}
        {message && <p className="operation-message" role="status">{message}</p>}
        {error && <p className="error-text" role="alert">{error}</p>}
      </main>
    </div>}

    {workbenchPort && <div className="embedded-workbench">
      <div className="desktop-bar"><Button variant="plain" className="back-to-variants" onClick={()=>{ setWorkbenchPort(null); void refresh(); }}><Icon name="chevron" size={14}/>域能力</Button><span className="desktop-separator"/><span>{INSTALL_NAME}</span></div>
      <iframe key={workbenchPort} title={`${INSTALL_NAME} 工作台`} src={`http://127.0.0.1:${workbenchPort}/`}/>
    </div>}

    <ConfirmDialog
      open={!!confirm}
      onClose={()=>setConfirm(null)}
      onConfirm={confirmAction}
      title={confirm?.kind === 'start' ? '另一个安装正在运行' : confirm?.kind === 'toggle' ? `关闭 ${confirm.domain.title}` : confirm?.title || ''}
      confirmLabel={confirm?.kind === 'start' ? '停止它并启动' : confirm?.kind === 'toggle' ? '仍然关闭' : confirm?.label || '确定'}
      variant={confirm?.kind === 'action' && confirm.action === 'install_variant' ? 'primary' : 'danger'}>
      {confirm?.kind === 'toggle'
        ? confirm.detail
        : confirm?.kind === 'start'
          ? <div className="foreign-warning">
              <p>同一时间只能运行一个实例。要继续启动，需要先停止下面这个安装：</p>
              <ul>{confirm.foreign.map(item => <li key={item.pid}><code>{item.path}</code></li>)}</ul>
              <p className="risk">停止会中断它正在运行的任务，Kernel 及其管理的域会被结束。数据不会丢失，但未完成的任务需要重新跑。</p>
            </div>
          : confirm?.body}
    </ConfirmDialog>
  </>;
}

createRoot(document.getElementById('root')).render(<App/>);
