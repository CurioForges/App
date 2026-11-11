import React, {useEffect, useRef, useState} from 'react';

/** 等环境构建时的小恐龙:空格/点击跳跃,撞上就结束,按空格或点击重开。
 *  只画方块,不引任何资源 —— 它的唯一职责是让几十秒的等待不那么难熬。
 *  画布跟着容器宽度走:地面线横贯整行,障碍从窗口右缘进入。 */
const H = 80;
const GROUND = H - 16;
const PLAYER = {x: 44, w: 14, h: 22};
const GRAVITY = 0.9;
const JUMP = -9.4;

export function DinoGame() {
  const canvasRef = useRef(null);
  const wrapRef = useRef(null);
  const width = useRef(520);
  const state = useRef({
    y: 0, vy: 0, jumping: false, speed: 3.2, score: 0,
    obstacles: [], nextSpawn: 60, over: false, started: false,
  });
  const [score, setScore] = useState(0);
  const [over, setOver] = useState(false);

  function reset(s) {
    s.y = 0; s.vy = 0; s.jumping = false; s.speed = 3.2; s.score = 0;
    s.obstacles = []; s.nextSpawn = 60; s.over = false; s.started = true;
    setScore(0); setOver(false);
  }

  function jump(s) {
    if (s.over) { reset(s); return }
    s.started = true;
    if (!s.jumping) { s.vy = JUMP; s.jumping = true }
  }

  useEffect(() => {
    const canvas = canvasRef.current;
    const wrap = wrapRef.current;
    if (!canvas || !wrap) return;
    const ctx = canvas.getContext('2d');
    const s = state.current;
    let raf = 0;

    // 画布尺寸跟容器走(按 DPR 放大再缩放回来,线条才不糊)。
    const fit = () => {
      const dpr = window.devicePixelRatio || 1;
      const css = Math.max(320, Math.round(wrap.clientWidth || 520));
      width.current = css;
      canvas.width = Math.round(css * dpr);
      canvas.height = Math.round(H * dpr);
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    };
    fit();
    const observer = new ResizeObserver(fit);
    observer.observe(wrap);

    const onKey = event => {
      if (event.code === 'Space' || event.code === 'ArrowUp') {
        event.preventDefault();
        jump(state.current);
      }
    };
    const onTap = () => jump(state.current);
    window.addEventListener('keydown', onKey);
    canvas.addEventListener('pointerdown', onTap);

    const tick = () => {
      const W = width.current;
      // 物理
      if (s.jumping) {
        s.vy += GRAVITY;
        s.y += s.vy;
        if (s.y >= 0) { s.y = 0; s.vy = 0; s.jumping = false }
      }
      // 障碍
      if (s.started && !s.over) {
        s.nextSpawn -= s.speed;
        if (s.nextSpawn <= 0) {
          const h = 14 + Math.random() * 12;
          s.obstacles.push({x: W + 10, w: 9 + Math.random() * 8, h});
          s.nextSpawn = 70 + Math.random() * 90;
        }
        for (const o of s.obstacles) o.x -= s.speed;
        s.obstacles = s.obstacles.filter(o => o.x + o.w > -20);
        s.speed = Math.min(9, 3.2 + s.score / 220);
        s.score += 0.25;
        setScore(Math.floor(s.score));

        const py = GROUND - PLAYER.h + s.y;
        for (const o of s.obstacles) {
          const hit = PLAYER.x < o.x + o.w && PLAYER.x + PLAYER.w > o.x
            && py + PLAYER.h > GROUND - o.h;
          if (hit) { s.over = true; setOver(true) }
        }
      }

      // 画:地面线横贯整个画布,不留内边距
      ctx.clearRect(0, 0, W, H);
      ctx.strokeStyle = 'rgba(120,116,110,.45)';
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.moveTo(0, GROUND + 0.5);
      ctx.lineTo(W, GROUND + 0.5);
      ctx.stroke();

      ctx.fillStyle = '#2F6F4E';
      for (const o of s.obstacles) {
        ctx.fillRect(o.x, GROUND - o.h, o.w, o.h);
      }
      const py = GROUND - PLAYER.h + s.y;
      ctx.fillStyle = '#1A1A18';
      ctx.fillRect(PLAYER.x, py, PLAYER.w, PLAYER.h);

      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => {
      cancelAnimationFrame(raf);
      observer.disconnect();
      window.removeEventListener('keydown', onKey);
      canvas.removeEventListener('pointerdown', onTap);
    };
  }, []);

  return <div className="dino">
    <div className="dino-stage" ref={wrapRef}>
      <canvas ref={canvasRef} aria-label="等待时的小游戏"/>
    </div>
    <div className="dino-meta">
      {over
        ? <span>撞到了 · 按空格或点击重来</span>
        : <span>{score > 0 ? `${score}` : '按空格 / 点击 跳跃'}</span>}
    </div>
  </div>;
}
