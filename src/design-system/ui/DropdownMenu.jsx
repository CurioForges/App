import React,{useId,useRef,useState} from 'react';
import {Button} from './forms';
import {FloatingPanel} from './FloatingPanel';

/** Actions are supplied by callers. A single available action executes directly. */
export function DropdownMenu({children,items=[],disabled=false,label='操作菜单',className='',title,onAction}) {
  const [open,setOpen]=useState(false);
  const triggerRef=useRef(null),menuRef=useRef(null),initialFocus=useRef(0);
  const id=useId();
  const actions=items.filter(item=>!item.hidden);
  const close=()=>setOpen(false);
  const run=item=>{
    if(item.disabled) return;
    close();triggerRef.current?.focus();
    (item.onSelect||(()=>onAction?.(item.value)))();
  };
  const focusItem=index=>{
    const buttons=menuRef.current?.querySelectorAll('[role="menuitem"]:not(:disabled)');
    if(buttons?.length) buttons[(index+buttons.length)%buttons.length]?.focus();
  };
  return <>
    <Button ref={triggerRef} variant="plain" className={className} disabled={disabled||!actions.length} title={title} aria-haspopup={actions.length>1?'menu':undefined} aria-expanded={actions.length>1?open:undefined} aria-controls={open?id:undefined}
      onClick={()=>{if(actions.length===1) run(actions[0]);else {initialFocus.current=0;setOpen(previous=>!previous);}}}
      onKeyDown={event=>{
        if(actions.length>1&&['ArrowDown','ArrowUp'].includes(event.key)) {event.preventDefault();initialFocus.current=event.key==='ArrowUp'?-1:0;setOpen(true);}
      }}>{children}</Button>
    <FloatingPanel anchorRef={triggerRef} open={open&&!disabled&&actions.length>1} onDismiss={close} onReady={()=>focusItem(initialFocus.current)} align="end" id={id} role="menu" aria-label={label} className="dropdown-menu">
      <div ref={menuRef}
        onKeyDown={event=>{
          const buttons=[...menuRef.current.querySelectorAll('[role="menuitem"]:not(:disabled)')];
          const index=buttons.indexOf(document.activeElement);
          if(['ArrowDown','ArrowUp','Home','End'].includes(event.key)) {event.preventDefault();focusItem(event.key==='Home'?0:event.key==='End'?buttons.length-1:index+(event.key==='ArrowDown'?1:-1));}
          else if(event.key==='Tab') {
            event.preventDefault();close();
            const root=triggerRef.current.closest('dialog')||document;
            const controls=[...root.querySelectorAll('button,a[href],input,select,textarea,[tabindex]')].filter(node=>node.tabIndex>=0&&!node.disabled&&node.getClientRects().length&&!node.closest('.ui-popup'));
            const index=controls.indexOf(triggerRef.current);
            (controls[index+(event.shiftKey?-1:1)]||triggerRef.current)?.focus();
          }
        }}>
        {actions.map(item=><Button key={item.value} variant="plain" role="menuitem" className={`ui-popup-option ${item.danger?'danger':''}`} disabled={item.disabled} onClick={()=>run(item)}>{item.label}</Button>)}
      </div>
    </FloatingPanel>
  </>;
}
