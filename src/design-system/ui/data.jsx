import React,{useEffect,useId,useRef,useState} from 'react';
import {Icon} from './icons';
import {Button} from './forms';
import {Panel} from './layout';

export const DataTable = ({columns,rows,rowKey='id'}) => <div className="table-scroll"><table><thead><tr>{columns.map(c=><th key={c.key}>{c.label}</th>)}</tr></thead><tbody>{rows.map(r=><tr key={r[rowKey]}>{columns.map(c=><td key={c.key}>{c.render?c.render(r[c.key],r):r[c.key]}</td>)}</tr>)}</tbody></table></div>;

export const ListCard = ({title,description,action,icon='folder'}) => <Panel className="list-card"><span className="icon-box"><Icon name={icon}/></span><div><b>{title}</b><p>{description}</p></div>{action}</Panel>;

export const Badge = ({ children,tone='neutral' }) => <span className={`badge ${tone}`}><i/>{children}</span>;

export const Metric = ({label,value,detail}) => <div className="metric"><span>{label}</span><strong>{value}</strong><small>{detail}</small></div>;

export const Progress = ({value,label='进度',indeterminate=false,indeterminateLabel='等待中',tone='success'}) => <div className="progress-wrap"><div className="progress-meta"><span>{label}</span><b>{indeterminate?indeterminateLabel:Number.isFinite(value)?`${value}%`:'—'}</b></div><div className={`progress tone-${tone} ${indeterminate?'indeterminate':''}`} role="progressbar" aria-label={label} aria-valuenow={indeterminate||!Number.isFinite(value)?undefined:value} aria-valuemin={0} aria-valuemax={100}><span style={{width:indeterminate?'35%':`${Number.isFinite(value)?value:0}%`}}/></div></div>;

export const Pagination = ({page,total,onChange}) => <div className="pagination"><span>第 {page} / {total} 页</span><Button size="small" disabled={page===1} onClick={()=>onChange(page-1)}>上一页</Button><Button size="small" disabled={page===total} onClick={()=>onChange(page+1)}>下一页</Button></div>;

export const DetailList = ({items}) => <dl className="details">{items.map(([k,v])=><React.Fragment key={k}><dt>{k}</dt><dd>{v}</dd></React.Fragment>)}</dl>;

export const Thumbnail = ({children,label}) => <div className="thumbnail" role="img" aria-label={label}>{children}</div>;
