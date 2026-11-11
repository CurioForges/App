import React,{useEffect,useId,useRef,useState} from 'react';
import {Icon} from './icons';

export const AppShell = ({sidebar,header,children}) => <div className="app-shell"><aside className="sidebar">{sidebar}</aside><div className="workspace">{header}<main>{children}</main></div></div>;

export const SidebarNav = ({items,active,onChange}) => <nav className="side-nav" aria-label="组件分类">{items.map(i=><button key={i.id} aria-label={`${i.label}${i.count?' '+i.count:''}`} title={i.label} aria-current={active===i.id?'location':undefined} className={active===i.id?'active':''} onClick={()=>onChange(i.id)}><Icon name={i.icon}/><span>{i.label}</span><small>{i.count}</small></button>)}</nav>;

export const PageHeader = ({eyebrow,title,children,action}) => <header className="page-head"><div><div className="eyebrow">{eyebrow}</div><h1>{title}</h1>{children&&<p>{children}</p>}</div>{action}</header>;

export const Panel = ({children,className='',ref,...props}) => <div ref={ref} className={`panel ${className}`} {...props}>{children}</div>;

export const Toolbar = ({children}) => <div className="toolbar">{children}</div>;

export const SplitPane = ({children}) => <div className="split">{children}</div>;
