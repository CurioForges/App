import React, {useEffect, useRef, useState} from 'react';
import {createRoot} from 'react-dom/client';
import {invoke} from '@tauri-apps/api/core';
import {listen} from '@tauri-apps/api/event';
import {Panel} from './design-system/ui/layout';
import {Button, Input} from './design-system/ui/forms';
import {Badge} from './design-system/ui/data';
import {ConfirmDialog, Modal} from './design-system/ui/feedback';
import {DropdownMenu} from './design-system/ui/DropdownMenu';
import {Icon} from './design-system/ui/icons';
import {AtelierBrand} from './design-system/AtelierBrand';
import {Setup} from './views/Setup';
import {DinoGame} from './DinoGame';
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

/** 卡片头部那条占用比例条:三段落,文本贴在段内,太窄时逐级收缩,0 段不渲染。 */
function formatBytes(bytes) {
  const mb = bytes / 1048576;
  if (mb >= 1024) return `${(mb / 1024).toFixed(1)} GB`;
  if (mb >= 10) return `${Math.round(mb)} MB`;
  return `${mb.toFixed(1)} MB`;
}

function StorageBar({usage}) {
  const ref = useRef(null);
  const [width, setWidth] = useState(0);
  const total = usage?.total || 0;
  const segments = (usage?.segments || []).filter(segment => segment.bytes > 0 && total > 0);

  // 段的实际像素宽度随窗口变化:按实测宽度决定文本露出几段,保证绝不越过区间边界。
  useEffect(() => {
    const node = ref.current;
    if (!node) return;
    const read = () => setWidth(node.clientWidth);
    read();
    const observer = new ResizeObserver(read);
    observer.observe(node);
    return () => observer.disconnect();
  }, [segments.length, total]);

  if (!usage || (segments.length === 0 && !usage.scanning)) return null;
  if (segments.length === 0) {
    return <div className="storage-bar is-scanning" role="status" title="正在统计三个目录的占用"><span>统计中…</span></div>;
  }
  return <div className={`storage-bar ${usage.scanning ? 'is-stale' : ''}`} ref={ref} role="img"
    title={usage.scanning ? '正在重新统计，显示的是上一次的结果' : undefined}
    aria-label={segments.map(segment => `${segment.label} ${formatBytes(segment.bytes)}`).join('，')}>
    {segments.map(segment => {
      const percent = (segment.bytes / total) * 100;
      const span = Math.max(0, Math.round(width * (segment.bytes / total)));
      // 三段文字从完整到只剩百分比逐级退化,阈值按实际字宽留了内边距的余量。
      const detail = span >= 150 ? 'full' : span >= 104 ? 'size' : span >= 54 ? 'pct' : 'none';
      return <div key={segment.key} className={`storage-seg seg-${segment.key}`}
        style={{flexGrow: segment.bytes}}
        title={`${segment.label} ${formatBytes(segment.bytes)} · ${percent.toFixed(1)}%`}>
        {detail !== 'none' && <span className="storage-seg-text">
          {detail === 'full' && <span>{segment.label}</span>}
          {detail === 'full' && <span>{formatBytes(segment.bytes)}</span>}
          <span>{percent.toFixed(1)}%</span>
        </span>}
      </div>;
    })}
  </div>;
}


const PHASE_TEXT = {
  resolve: '获取中', plan: '准备中', download: '下载中', verify: '校验中',
  stage: '解压中', swap: '换入中', env: '构建环境', start: '启动中',
  check: '检查中', uninstall: '卸载中', done: '完成', fail: '失败',
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
      {/* 速率一直显示:编译 wheel 时是 0.0 MB/s,数字不闪不掉,用户能看出"在动/没在动" */}
      {typeof progress.rate === 'number' && <span className="update-rate">{`${(progress.rate / 1048576).toFixed(1)} MB/s`}</span>}
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
  const [installing, setInstalling] = useState(false);
  const [storage, setStorage] = useState(null);
  const [locations, setLocations] = useState(null);
  const [moveChoice, setMoveChoice] = useState(null);
  const [moving, setMoving] = useState(false);
  const [moveError, setMoveError] = useState('');
  const [dev, setDev] = useState(false);

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

  // 占用统计:后台扫完一次就广播一次,界面只读缓存的结果。
  useEffect(() => {
    let off;
    (async () => {
      off = await listen('atelier://storage', event => setStorage(event.payload || null));
      try { setStorage(await invoke('storage_usage')); } catch { /* 首次还没扫完 */ }
    })();
    return () => { off?.(); };
  }, []);

  // 只有开发构建才露出「重新运行初始设置」这类入口。
  useEffect(() => {
    invoke('build_info').then(info => setDev(!!info?.debug)).catch(() => setDev(false));
  }, []);

  async function refresh() {
    try {
      const [nextCap, nextShared, nextLocations] = await Promise.all([
        invoke('capabilities'), invoke('shared_settings'), invoke('locations_view'),
      ]);
      setCap(nextCap);
      setShared(nextShared);
      setLocations(nextLocations);
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

  /** 选完新位置先问一句：搬家还是只切路径。搬得多、重下得少，代价写清楚让人自己选。 */
  async function chooseLocation(kind) {
    try {
      const current = kind === 'install' ? locations?.install : locations?.data;
      const picked = await invoke('choose_directory', {kind, current: current || null});
      if (!picked) return;
      if (picked.toLowerCase() === String(current || '').toLowerCase()) return;
      setMoveError('');
      setMoveChoice({kind, target: picked});
    } catch (err) { setError(String(err)); }
  }

  /** 权重目录：可以搬过去，也可以只切指针（新位置已经有同一套权重时）。 */
  async function chooseModels() {
    try {
      const picked = await invoke('choose_directory', {kind:'models', current: shared?.modelsDir || locations?.models || null});
      if (!picked) return;
      if (picked.toLowerCase() === String(shared?.modelsDir || '').toLowerCase()) return;
      setMoveError('');
      setMoveChoice({kind:'models', target: picked});
    } catch (err) { setError(String(err)); }
  }

  async function runRelocate(mode) {
    if (!moveChoice) return;
    setMoving(true); setMessage(''); setError(''); setMoveError('');
    try {
      const report = moveChoice.kind === 'install'
        ? await invoke('relocate_install', {target: moveChoice.target, mode})
        : moveChoice.kind === 'models'
          ? await invoke('relocate_models', {target: moveChoice.target, mode})
          : await invoke('relocate_data', {target: moveChoice.target});
      setMoveChoice(null);
      setMessage(report?.message || '已迁移完成。');
      await refresh();
    } catch (err) { setMoveError(String(err)); }
    finally { setMoving(false); }
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
  /** 引导结束后的第一段：首次安装 → 卡片页 → 预热 → 控制台。 */
  async function afterSetup() {
    setNeedsSetup(false);
    setError('');
    try {
      await refresh();
      const info = await invoke('capabilities');
      if (!info.installed) {
        setInstalling(true);
        await invoke('install_only');
        setInstalling(false);
        await refresh();
      }
      await new Promise(resolve => setTimeout(resolve, 1200));
      const port = await invoke('prewarm_only');
      if (port) setWorkbenchPort(port);
    } catch (err) {
      setInstalling(false);
      setError(String(err));
    }
  }
  const menuItems = [
    {value:'data', label:'打开数据目录', onSelect:()=>void invoke('open_runtime_path', {kind:'data'})},
    {value:'models', label:'打开模型目录', onSelect:()=>void invoke('open_runtime_path', {kind:'models'})},
    // 发布版不带"回到首次向导"的入口:它会按新目录把整套组件重装一遍,只留给开发调试。
    ...(dev ? [{value:'setup', label:'重新运行初始设置', onSelect:()=>void invoke('reopen_setup')}] : []),
    {value:'update', label:'更新发行包', onSelect:()=>setConfirm({kind:'action', action:'install_variant', title:'更新 Atelier Free', body:'更新会停止当前实例，运行中的任务可能中断。', label:'更新'})},
    {value:'prewarm', label:'准备运行环境', onSelect:()=>run('prewarm_only')},
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
  if (needsSetup) return <Setup onDone={afterSetup}/>;

  return <>
    {installing && progress && <div className="install-screen"><UpdateOverlay progress={progress} log={updateLog}/></div>}
    {!workbenchPort && <div className="launcher">
      <header className="launcher-header"><AtelierBrand/><div className="header-actions">
        <button type="button" className={`header-icon ${view === 'settings' ? 'active' : ''}`} aria-label="全局设置" title="全局设置" onClick={()=>setView(view === 'settings' ? 'capabilities' : 'settings')}><Icon name="sliders" size={16}/></button>
        <div className="header-context"><span className="tiny-dot"/>桌面工作台</div>
      </div></header>
      <main className="launcher-main">
        <div className="page-intro"><div><div className="eyebrow">ATELIER / CAPABILITIES</div><h1>域能力</h1><p>每一项是一个能力单元；这里显示它是否安装、当前是否在这个实例里运行。</p></div></div>

        {view === 'capabilities' && <Panel className="instance-card">
          {/* 迁移进度画在设置弹窗里，不要再用全局覆盖层盖住界面。 */}
          {progress && progress.job !== 'relocate' && <UpdateOverlay progress={progress} log={updateLog}/>}
          <div className="instance-head">
            <img className="variant-mark" src={freeMark} alt=""/>
            <div className="instance-title">
              <h2>{INSTALL_NAME}</h2>
              <span className="card-kind">{cap?.version ? `v${cap.version}` : '版本未知'}{installed ? ` · ${cap?.installDir}` : ' · 尚未安装'}</span>
            </div>
            {/* 占用条占住「运行中」前边的全部空位 */}
            <StorageBar usage={storage}/>
            <div className="instance-state">
              <Badge tone={running ? 'success' : 'neutral'}>{!installed ? '未安装' : running ? '运行中' : '已停止'}</Badge>
            </div>
            <div className="instance-actions">
              {installed
                ? running
                  ? <Button size="small" onClick={()=>setConfirm({kind:'action', action:'stop_variant', title:'停止 Atelier Free', body:'停止会中断正在运行的任务。Kernel 会关闭并回收它管理的域。', label:'停止'})} disabled={!!busy}>停止</Button>
                  : <Button size="small" variant="primary" onClick={startInstall} disabled={!!busy}>{busy === 'open_workbench' ? '启动中…' : '启动'}</Button>
                : <Button size="small" variant="primary" onClick={()=>run('install_variant')} disabled={!!busy}>安装</Button>}
              {installed && running && <Button size="small" variant="primary" onClick={()=>run('open_workbench')} disabled={!!busy}>打开工作台</Button>}
              <DropdownMenu className="card-menu" label={`${INSTALL_NAME} 操作`} title={`${INSTALL_NAME} 操作`} disabled={!!busy} items={menuItems}><Icon name="gear" size={16}/></DropdownMenu>
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
              <span className="setting-label">安装位置</span>
              <div className="setting-path" title={locations?.install || undefined}>{locations?.install || '默认位置'}</div>
              <Button size="small" onClick={()=>chooseLocation('install')} disabled={moving || !installed}>更改</Button>
            </div>
            <div className="setting-bar">
              <span className="setting-label">数据目录</span>
              <div className="setting-path" title={locations?.data || undefined}>
                {locations?.data || '安装目录默认位置'}
                {locations?.dataLinked ? <span className="setting-hint">（联接）</span> : null}
              </div>
              <Button size="small" onClick={()=>chooseLocation('data')} disabled={moving || !installed}>更改</Button>
            </div>
            <div className="setting-bar">
              <span className="setting-label">权重目录</span>
              <div className="setting-path">{shared?.modelsDir || shared?.modelsDirEffective || '安装目录默认位置'}</div>
              <Button size="small" onClick={chooseModels} disabled={moving}>更改</Button>
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
      {/* 小游戏在卡片外面:覆盖层挡住的是卡片,等待时总得有点能动手的东西 */}
      {progress?.phase === 'env' && <div className="dino-dock"><DinoGame/></div>}
    </div>}

    {workbenchPort && <div className="embedded-workbench">
      <div className="desktop-bar"><Button variant="plain" className="back-to-variants" onClick={()=>{ setWorkbenchPort(null); void refresh(); }}><Icon name="chevron" size={14}/>域能力</Button><span className="desktop-separator"/><span>{INSTALL_NAME}</span></div>
      <iframe key={workbenchPort} title={`${INSTALL_NAME} 工作台`} src={`http://127.0.0.1:${workbenchPort}/`}/>
    </div>}

    <Modal
      open={!!moveChoice}
      onClose={()=>{ if (!moving) setMoveChoice(null); }}
      title={moveChoice?.kind === 'install' ? '换安装位置'
        : moveChoice?.kind === 'models' ? '换权重目录' : '换数据目录'}
      footer={<>
        <Button onClick={()=>setMoveChoice(null)} disabled={moving}>取消</Button>
        {moveChoice?.kind === 'install' && <Button onClick={()=>runRelocate('reinstall')} disabled={moving}>
          {moving ? '处理中…' : '在新位置重新安装'}
        </Button>}
        {moveChoice?.kind === 'models' && <Button onClick={()=>runRelocate('switch')} disabled={moving}>
          {moving ? '处理中…' : '只切换路径'}
        </Button>}
        <Button variant="primary" onClick={()=>runRelocate('move')} disabled={moving}>
          {moving ? '处理中…'
            : moveChoice?.kind === 'install' ? '迁移到新位置'
            : moveChoice?.kind === 'models' ? '迁移权重' : '迁移数据'}
        </Button>
      </>}>
      <div className="relocate-body">
        <div className="relocate-paths">
          <span>从</span><code title={moveChoice?.kind === 'models' ? locations?.models : locations?.install}>
            {moveChoice?.kind === 'models'
              ? (locations?.models || '当前权重目录')
              : (locations?.install || '当前安装位置')}</code>
          <span>到</span><code title={moveChoice?.target}>{moveChoice?.target}</code>
        </div>
        {moveError ? <p className="error-text" role="alert">{moveError}</p> : null}
        {moving && (() => {
          const moving_ = progress?.job === 'relocate' ? progress : null;
          const pct = typeof moving_?.percent === 'number' ? Math.max(0, Math.min(100, moving_.percent)) : null;
          const gb = value => ((value || 0) / 1e9).toFixed(1);
          // 剩余时间用「剩余字节 ÷ 平滑后的速率」；速率是后端做过指数平滑的，不会来回跳。
          const remaining = moving_?.total && moving_.rate > 0
            ? Math.max(0, moving_.total - (moving_.done || 0)) / moving_.rate
            : null;
          const etaText = remaining == null ? null
            : remaining < 60 ? '不到 1 分钟'
            : remaining < 3600 ? `约 ${Math.max(1, Math.round(remaining / 60))} 分钟`
            : `约 ${Math.floor(remaining / 3600)} 小时 ${Math.round((remaining % 3600) / 60)} 分`;
          return <div className="relocate-progress" role="status" aria-live="polite">
            <div className={`relocate-bar ${pct == null ? 'is-indeterminate' : ''}`}>
              <span style={pct == null ? undefined : {width: `${pct}%`}}/>
            </div>
            <div className="relocate-stats">
              <span>{pct == null ? (moving_?.label || '准备中…') : `${Math.round(pct)}%`}</span>
              {moving_?.total
                ? <span>{`${gb(moving_.done)} / ${gb(moving_.total)} GB`}</span>
                : moving_ ? <span>{`已搬 ${gb(moving_.done)} GB`}</span> : null}
              {typeof moving_?.rate === 'number'
                ? <span>{`${(moving_.rate / 1048576).toFixed(1)} MB/s`}</span>
                : null}
              {etaText ? <span className="relocate-eta">{`还剩 ${etaText}`}</span> : null}
            </div>
            {moving_?.detail ? <p className="relocate-detail">{moving_.detail}</p> : null}
          </div>;
        })()}
        {!moving && (moveChoice?.kind === 'install' ? <>
          <p><b>迁移</b>：把现有安装整体搬过去，引擎环境跟着走（构建好的环境会重写路径并逐个自检，
            自检不过的下次用到时重建）。新位置验证通过后才清空旧目录。</p>
          <p><b>重新安装</b>：在新位置按清单重新下载装配，不搬任何文件；完成后清空旧目录。</p>
          <p className="risk">两种方式都会先停止正在运行的实例，未完成的任务需要重新跑。</p>
        </> : moveChoice?.kind === 'models' ? <>
          <p><b>迁移权重</b>：把整个权重目录搬过去（`hf_cache` 里的 blobs/snapshots 结构照样保留，
            搬完用硬链接补回快照指针，不额外占空间）。新位置放得下再开始，通常几十到上百 GB。</p>
          <p><b>只切换路径</b>：新位置已经有一套权重（比如另一台安装留下的）时用这个，
            只改设置，不搬也不删。</p>
          <p className="risk">迁移会先停止正在运行的实例；旧目录如果在安装根之外，不会替你删。</p>
        </> : <>
          <p>把现有数据（任务记录、管线产物、数据库）搬到新位置，然后把
            <code>{locations?.install}\data</code> 指过去；旧目录迁移成功后清空。</p>
          <p className="risk">会先停止正在运行的实例，未完成的任务需要重新跑。</p>
        </>)}
      </div>
    </Modal>

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
