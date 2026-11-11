import React,{useLayoutEffect,useRef,useState} from 'react';
import {createPortal} from 'react-dom';

/** Shared, viewport-constrained popup. Portals stay in the active modal's top layer. */
export function FloatingPanel({anchorRef,boundaryRef,open,onDismiss,onReady,align='start',className='',children,...props}) {
  const panelRef=useRef(null);
  const dismissRef=useRef(onDismiss);
  const readyRef=useRef(onReady);
  dismissRef.current=onDismiss;
  readyRef.current=onReady;
  const [position,setPosition]=useState(null);
  useLayoutEffect(()=>{
    if(!open) return;
    const anchor=anchorRef.current;
    if(!anchor) return;
    const panel=panelRef.current;
    const viewport=window.visualViewport;
    const reposition=()=>{
      const rect=anchor.getBoundingClientRect();
      const leftEdge=viewport?.offsetLeft||0,topEdge=viewport?.offsetTop||0;
      const rightEdge=leftEdge+(viewport?.width||window.innerWidth);
      const bottomEdge=topEdge+(viewport?.height||window.innerHeight);
      const width=Math.min(Math.max(rect.width,160),rightEdge-leftEdge-16);
      const below=bottomEdge-rect.bottom-12,above=rect.top-topEdge-12;
      const flip=below<Math.min(panel?.scrollHeight||240,240)&&above>below;
      const maxHeight=Math.max(0,Math.min(280,flip?above:below));
      const height=Math.min(panel?.scrollHeight||0,maxHeight);
      setPosition({width,maxHeight,left:Math.max(leftEdge+8,Math.min(align==='end'?rect.right-width:rect.left,rightEdge-width-8)),top:flip?rect.top-height-4:rect.bottom+4,visibility:'visible'});
    };
    reposition();
    const frame=requestAnimationFrame(()=>readyRef.current?.());
    const observer=new ResizeObserver(reposition);
    observer.observe(anchor);
    if(panel) observer.observe(panel);
    const outside=event=>{
      if(!(boundaryRef?.current||anchor).contains(event.target)&&!panel?.contains(event.target)) dismissRef.current?.();
    };
    const escape=event=>{
      if(event.key==='Escape') {event.preventDefault();event.stopPropagation();dismissRef.current?.();anchor.focus();}
    };
    document.addEventListener('pointerdown',outside,true);
    document.addEventListener('focusin',outside,true);
    document.addEventListener('keydown',escape,true);
    window.addEventListener('resize',reposition);
    window.addEventListener('scroll',reposition,true);
    viewport?.addEventListener('resize',reposition);
    viewport?.addEventListener('scroll',reposition);
    return ()=>{
      cancelAnimationFrame(frame);
      observer.disconnect();
      document.removeEventListener('pointerdown',outside,true);
      document.removeEventListener('focusin',outside,true);
      document.removeEventListener('keydown',escape,true);
      window.removeEventListener('resize',reposition);
      window.removeEventListener('scroll',reposition,true);
      viewport?.removeEventListener('resize',reposition);
      viewport?.removeEventListener('scroll',reposition);
    };
  },[open,anchorRef,boundaryRef,align]);
  if(!open||!anchorRef.current) return null;
  return createPortal(<div {...props} ref={panelRef} className={`ui-popup ${className}`} style={position||{visibility:'hidden'}}>{children}</div>,anchorRef.current.closest('dialog')||document.body);
}
