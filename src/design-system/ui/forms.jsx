import React,{useId,useState} from 'react';
import {Icon} from './icons';

export const Button = ({ children, variant='secondary', size='', icon,className='', ...props }) => <button className={variant==='plain'?`control-button ${className}`:`btn ${variant} ${size} ${className}`} type="button" {...props}>{icon && <Icon name={icon}/>} {children}</button>;

export const Input = ({className='',...props}) => <input className={`input ${className}`} {...props}/>;

export const Textarea = ({className='',...props}) => <textarea className={`input ${className}`} rows={3} {...props}/>;

export const NumberInput = props => <Input type="number" {...props}/>;

export {Select,ComboBox} from './choices';

export const Switch = ({ checked, onChange, label }) => <label className="switch-label"><button type="button" role="switch" aria-checked={checked} aria-label={label} className={`switch ${checked?'on':''}`} onClick={()=>onChange(!checked)}><span/></button>{label}</label>;

const ChoiceInput = ({type,label,className='',...props}) => {
  const input=<input type={type} className={`choice-input ${className}`} {...props}/>;
  return label==null?input:<label className="check">{input}<span>{label}</span></label>;
};
export const Checkbox = props => <ChoiceInput {...props} type="checkbox"/>;
export const Radio = props => <ChoiceInput {...props} type="radio"/>;

export function Field({label,help,error,children,className='',group=false}) {
  const generatedId=useId();
  const id=(!group&&React.isValidElement(children)&&children.props.id)||generatedId;
  const description=help||error?`${id}-help`:undefined;
  return <div className={`field ${error?'invalid':''} ${className}`} role={group?'group':undefined} aria-labelledby={group?`${id}-label`:undefined} aria-describedby={group?description:undefined}>
    {group?<span id={`${id}-label`} className="field-label">{label}</span>:<label htmlFor={id}>{label}</label>}
    {!group&&React.isValidElement(children)?React.cloneElement(children,{id,'aria-describedby':[children.props['aria-describedby'],description].filter(Boolean).join(' ')||undefined,'aria-invalid':!!error}):children}
    {(help||error)&&<small id={`${id}-help`}>{error||help}</small>}
  </div>;
}

/** 只负责文件选择；上传行为由 onFile 的调用方决定，组件不声明是否上传成功。 */
export function FileUpload({onFile,label='选择文件或拖放到此处',help='',accept,disabled=false}) {
  const [name,setName]=useState('');
  const receive=file=>{if(file&&!disabled){setName(file.name);onFile?.(file);}};
  return <label className="upload" aria-disabled={disabled} onDragOver={e=>e.preventDefault()} onDrop={e=>{e.preventDefault();receive(e.dataTransfer.files[0]);}}><Icon name="upload" size={24}/><b>{name||label}</b>{help&&<small>{help}</small>}<input aria-label={label} type="file" accept={accept} disabled={disabled} onChange={e=>receive(e.target.files[0])}/></label>;
}
