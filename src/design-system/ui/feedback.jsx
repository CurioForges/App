import React,{useEffect,useId,useRef,useState} from 'react';
import {Icon} from './icons';
import {Button} from './forms';

export function Modal({open,onClose,title,children,footer,drawer=false}) {
  const ref=useRef(); const titleId=useId();
  useEffect(()=>{if(!open)return;const previous=document.activeElement;const el=ref.current;el.showModal();document.body.style.overflow='hidden';return()=>{if(el.open)el.close();document.body.style.overflow='';previous?.focus();};},[open]);
  return <dialog ref={ref} className={drawer?'dialog drawer':'dialog'} aria-labelledby={titleId} onCancel={e=>{e.preventDefault();onClose();}} onClick={e=>{if(e.target===ref.current){const r=ref.current.getBoundingClientRect();if(e.clientX<r.left||e.clientX>r.right||e.clientY<r.top||e.clientY>r.bottom)onClose();}}}><div className="dialog-head"><h2 id={titleId}>{title}</h2><Button size="icon" aria-label="关闭弹窗" onClick={onClose}><Icon name="close"/></Button></div><div className="dialog-body">{children}</div>{footer&&<div className="dialog-footer">{footer}</div>}</dialog>;
}

export const Drawer = props => <Modal {...props} drawer/>;

export const ConfirmDialog = ({open,onClose,onConfirm,title='确认操作',children,confirmLabel='确定',cancelLabel='取消',variant='danger'}) => <Modal open={open} onClose={onClose} title={title} footer={<><Button onClick={onClose}>{cancelLabel}</Button><Button variant={variant} onClick={()=>{onConfirm();onClose();}}>{confirmLabel}</Button></>}>{children}</Modal>;

export function Toast({message,onClose}) { useEffect(()=>{if(!message)return;const t=setTimeout(onClose,3500);return()=>clearTimeout(t);},[message,onClose]); return message&&<div className="toast" role="status"><Icon name="check"/>{message}<Button variant="plain" aria-label="关闭提示" onClick={onClose}><Icon name="close" size={16}/></Button></div>; }

export const Tooltip = ({text,children}) => <span className="tooltip" tabIndex={0}>{children}<span role="tooltip">{text}</span></span>;

export const EmptyState = ({title='这里还没有内容',children,action}) => <div className="empty"><Icon name="layers" size={30}/><b>{title}</b><p>{children}</p>{action}</div>;

export const LoadingState = ({skeleton=false}) => skeleton?<div className="skeleton-stack" aria-label="正在加载" role="status"><i/><i/><i/></div>:<span className="loading" role="status"><i/>正在加载</span>;
