import React, {useEffect, useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {Button, Input} from '../design-system/ui/forms';
import {Badge} from '../design-system/ui/data';
import {PAGE_ORDER, orderedSteps} from '../setup-steps.mjs';
import {AtelierBrand} from '../design-system/AtelierBrand';
import dataFixedPath from '../assets/tutorials/data-1-fixed-path.svg';
import dataJunction from '../assets/tutorials/data-2-junction.svg';
import modelsVariable from '../assets/tutorials/models-1-variable.svg';
import modelsCaches from '../assets/tutorials/models-2-caches.svg';
import tokenSettings from '../assets/tutorials/token-1-settings.svg';
import tokenList from '../assets/tutorials/token-2-tokens.svg';
import tokenCreate from '../assets/tutorials/token-3-create.svg';
import tokenGated from '../assets/tutorials/token-4-gated.svg';

// Page set and display order live in ../setup-steps.mjs (asserted by tests).
const STEPS = orderedSteps();

function formatSize(gb) {
  if (gb == null) return null;
  return gb >= 100 ? `${Math.round(gb)} GB` : `${gb.toFixed(1)} GB`;
}

/** One horizontal strip: label on the left, the control in the middle, state at the end. */
function SettingBar({id, label, children, status, note}) {
  return <div className="setting-block">
    <div className="setting-bar">
      <span className="setting-label" id={id}>{label}</span>
      {children}
      {status && <div className="setting-status">{status}</div>}
    </div>
    {note && <p className="setting-note">{note}</p>}
  </div>;
}

function TutorialRow({items, columns}) {
  return <div className="tutorial-row" style={{'--columns':columns}}>
    {items.map(item => <figure className="tutorial-card" key={item.title}>
      <img src={item.image} alt={item.title}/>
      <figcaption>
        <span className="tutorial-step">{item.step}</span>
        <strong>{item.title}</strong>
        <small>{item.detail}</small>
      </figcaption>
    </figure>)}
  </div>;
}

export function Setup({onDone}) {
  const [status, setStatus] = useState(null);
  const [step, setStep] = useState(0);
  const [dataDir, setDataDir] = useState('');
  const [modelsDir, setModelsDir] = useState('');
  const [token, setToken] = useState('');
  const [dataCheck, setDataCheck] = useState(null);
  const [modelsCheck, setModelsCheck] = useState(null);
  const [tokenResult, setTokenResult] = useState(null);
  const [picking, setPicking] = useState('');
  const [modelsTouched, setModelsTouched] = useState(false);
  const [testing, setTesting] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');

  useEffect(() => {
    (async () => {
      try {
        const state = await invoke('setup_state');
        setStatus(state);
        // 第一栏问的是「安装与数据目录」,也就是安装根:默认给根目录,
        // 不能拿解析后的 `<root>/data` 回填,否则每次都会再套一层 data。
        setDataDir(state.defaults.installDir);
        setModelsDir(state.modelsDir);
      } catch (err) { setError(String(err)); }
    })();
  }, []);

  useEffect(() => {
    let cancelled = false;
    if (!dataDir) { setDataCheck(null); return; }
    invoke('inspect_directory', {kind:'data', path:dataDir})
      .then(result => { if (!cancelled) setDataCheck(result); })
      .catch(err => { if (!cancelled) setError(String(err)); });
    return () => { cancelled = true; };
  }, [dataDir]);

  useEffect(() => {
    let cancelled = false;
    if (!modelsDir) { setModelsCheck(null); return; }
    invoke('inspect_directory', {kind:'models', path:modelsDir})
      .then(result => { if (!cancelled) setModelsCheck(result); })
      .catch(err => { if (!cancelled) setError(String(err)); });
    return () => { cancelled = true; };
  }, [modelsDir]);

  async function choose(kind, current, apply) {
    setPicking(kind); setError('');
    try {
      const picked = await invoke('choose_directory', {kind, current:current || null});
      if (!picked) return;
      apply(picked);
      if (kind === 'models') { setModelsTouched(true); return; }
      // 权重目录默认跟着安装根走:用户还没单独挑过权重时,跟着新根改到 <root>\models,
      // 免得把权重留在旧盘的默认位置上。
      if (!modelsTouched) setModelsDir(picked.replace(/[\\/]+$/, '') + '\\models');
    } catch (err) { setError(String(err)); }
    finally { setPicking(''); }
  }

  async function testToken() {
    setTesting(true); setTokenResult(null); setError('');
    try { setTokenResult(await invoke('test_token', {token})); }
    catch (err) { setError(String(err)); }
    finally { setTesting(false); }
  }

  async function finish() {
    setBusy(true); setError('');
    try {
      await invoke('complete_setup', {dataDir, modelsDir, token});
      onDone();
    } catch (err) { setError(String(err)); }
    finally { setBusy(false); }
  }

  const blocked = PAGE_ORDER[step] === 'data'
    ? !dataDir || (dataCheck?.check?.blockers?.length || 0) > 0
    : PAGE_ORDER[step] === 'models'
      ? !modelsDir || (modelsCheck?.check?.blockers?.length || 0) > 0
      : false;

  function directoryBar({kind, value, label, check, apply}) {
    const detail = check?.check;
    const blockers = detail?.blockers || [];
    const advice = detail?.advice || [];
    return <SettingBar
      id={`setting-${kind}`}
      label={label}
      status={<>
        <Badge tone={blockers.length ? 'warning' : 'success'}>{blockers.length ? '需要处理' : '可用'}</Badge>
        {detail?.freeGb != null && <span className="setting-meta">可用 {formatSize(detail.freeGb)} / 共 {formatSize(detail.totalGb)}</span>}
        {detail?.onSystemDrive && <span className="setting-meta">系统盘</span>}
      </>}
      note={[...blockers, ...advice].join(' · ')}
    >
      <div className="setting-path" title={value || undefined}>{value || '还没有选择目录'}</div>
      <Button variant="primary" onClick={()=>choose(kind, value, apply)} disabled={!!picking}>
        {picking === kind ? '选择中…' : '选择目录'}
      </Button>
    </SettingBar>;
  }

  const pages = [
    {
      id:'data',
      eyebrow:'ATELIER / SETUP / 03',
      title:'安装与数据目录',
      lede:'应用本体、运行环境、任务记录和作品都放在这一个目录里。它会长到几十 GB，建议单独放一块盘，也最值得备份。',
      bar: directoryBar({kind:'data', value:dataDir, label:'安装与数据目录', check:dataCheck, apply:setDataDir}),
      columns: 2,
      items: [
        {image:dataFixedPath, step:'01 一个目录装全部', title:'组件、环境、作品都在这里', detail:'装完就是一台完整的机器,搬盘、备份、迁移只需要搬这一个目录。'},
        {image:dataJunction, step:'02 空间要够', title:'建议单独一块盘', detail:'组件约 4 GB,运行环境约 30 GB,再加上作品与缓存,留 80 GB 以上更从容。'},
      ],
    },
    {
      id:'models',
      eyebrow:'ATELIER / SETUP / 01',
      title:'模型权重目录',
      lede:'语音识别、说话人分离、翻译、图像生成都要加载权重。它们体积大、读取频繁，值得单独放一块盘。',
      bar: directoryBar({kind:'models', value:modelsDir, label:'权重目录', check:modelsCheck, apply:setModelsDir}),
      columns: 2,
      items: [
        {image:modelsVariable, step:'01 一个变量', title:'GCP_MODELS_DIR 决定权重在哪', detail:'引擎域读这个变量定位权重根，所以它可以放在任意磁盘，不需要跟着程序目录。'},
        {image:modelsCaches, step:'02 按来源分缓存', title:'所有引擎共用同一份', detail:'HF、torch、pyannote、ModelScope 各自建子目录，不会重复下载；换目录不会搬走旧权重。'},
      ],
    },
    {
      id:'token',
      eyebrow:'ATELIER / SETUP / 02',
      title:'权重下载 Token',
      lede:'公开权重可以直接下载，但说话人分离、声纹、扩散模型这些常用权重属于受限仓库：要带上你自己的令牌，并且在网页上同意过许可。',
      bar: <SettingBar
        id="setting-token"
        label="访问令牌"
        status={tokenResult
          ? <span className={tokenResult.valid ? 'token-ok' : 'error-text'}>{tokenResult.message}</span>
          : status?.tokenHint ? <span className="setting-meta">已保存 {status.tokenHint}</span> : null}
        note="用 Windows 数据保护加密保存在本机，只有当前 Windows 账号能解开 · 不写进配置文件、不出现日志 · 可以留空跳过"
      >
        <Input className="setting-input" type="password" value={token} onChange={event=>setToken(event.target.value)} placeholder="hf_…" autoComplete="off" spellCheck={false} aria-label="访问令牌"/>
        <Button onClick={testToken} disabled={testing || !token.trim()}>{testing ? '验证中…' : '测试连接'}</Button>
      </SettingBar>,
      columns: 4,
      items: [
        {image:tokenSettings, step:'01 登录', title:'头像 → Settings', detail:'登录 huggingface.co，点右上角头像，菜单里选 Settings。'},
        {image:tokenList, step:'02 进入列表', title:'Access Tokens', detail:'左侧菜单选 Access Tokens，再点右上角 New token。'},
        {image:tokenCreate, step:'03 创建', title:'选 Read 后复制', detail:'名字随意，类型选 Read；创建后立刻点 Copy，令牌只显示一次。'},
        {image:tokenGated, step:'04 同意许可', title:'gated 仓库点 Agree', detail:'每个受限仓库单独同意一次，用同一个账号，否则下载返回 403。'},
      ],
    },
  ];

  const page = pages.find(entry => entry.id === PAGE_ORDER[step]) || pages[0];

  return <div className="setup">
    <header className="setup-head">
      <div className="head-brand" onClick={event=>event.preventDefault()}><AtelierBrand/></div>
      <ol className="setup-steps">
        {STEPS.map((item, index) => <li key={item.id} className={index === step ? 'current' : index < step ? 'done' : ''}>
          <span className="step-index">{index + 1}</span>
          <span className="step-copy"><strong>{item.title}</strong><small>{item.caption}</small></span>
        </li>)}
      </ol>
      <span className="head-count">第 {step + 1} / {STEPS.length} 步</span>
    </header>

    <main className="setup-body">
      <div className="setup-intro">
        <div className="eyebrow">{page.eyebrow}</div>
        <h1>{page.title}</h1>
        <p className="lede">{page.lede}</p>
      </div>

      {page.bar}

      <TutorialRow items={page.items} columns={page.columns}/>
    </main>

    <footer className="setup-footer">
      <div className="footer-progress">
        {STEPS.map((item, index) => <span key={item.id} className={index <= step ? 'filled' : ''}/>)}
      </div>
      <div className="footer-actions">
        {error && <p className="error-text footer-error" role="alert">{error}</p>}
        {step > 0 && <Button onClick={()=>setStep(step - 1)} disabled={busy}>上一步</Button>}
        {step < 2
          ? <Button variant="primary" onClick={()=>setStep(step + 1)} disabled={blocked}>下一步</Button>
          : <Button variant="primary" onClick={finish} disabled={busy}>{busy ? '正在应用…' : '完成设置'}</Button>}
      </div>
    </footer>
  </div>;
}
