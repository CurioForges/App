import React,{useEffect,useId,useRef,useState} from 'react';
import {Icon} from './icons';
import {FloatingPanel} from './FloatingPanel';

const normalize=options=>options.map(item=>typeof item==='object'?{...item,value:String(item.value),label:item.label??String(item.value)}:{value:String(item),label:String(item)});

/** Select and editable ComboBox share options, focus, keyboard and popup behavior. */
function ChoiceControl({editable=false,options=[],value,defaultValue,placeholder='',className='',id:providedId,onChange,onFocus,onBlur,onKeyDown,disabled=false,required=false,name,form,...props}) {
  const generatedId=useId(),id=providedId||generatedId,listId=`${id}-options`;
  const triggerRef=useRef(null),nativeRef=useRef(null),boundaryRef=useRef(null);
  const [localValue,setLocalValue]=useState(defaultValue??(editable?'':normalize(options).find(item=>!item.disabled)?.value??''));
  const [open,setOpen]=useState(false),[active,setActive]=useState(null);
  const text=String(value??localValue);
  const items=normalize(options);
  const matches=editable?items.filter(item=>String(item.label).toLocaleLowerCase().includes(text.toLocaleLowerCase())):items;
  const expanded=open&&!disabled;
  const selected=items.find(item=>item.value===text);
  const activeIndex=matches.findIndex(item=>item.value===active&&!item.disabled);
  const close=()=>{setOpen(false);setActive(null);};
  const choose=item=>{
    if(item.disabled) return;
    setLocalValue(item.value);
    const target=editable?triggerRef.current:nativeRef.current;
    // Use the native event path so callers receive the same React change event as typing.
    const prototype=editable?HTMLInputElement.prototype:HTMLSelectElement.prototype;
    Object.getOwnPropertyDescriptor(prototype,'value').set.call(target,item.value);
    target.dispatchEvent(new Event(editable?'input':'change',{bubbles:true}));
    triggerRef.current?.focus();
    close();
  };
  useEffect(()=>{
    if(expanded&&activeIndex>=0) document.getElementById(`${listId}-${activeIndex}`)?.scrollIntoView({block:'nearest'});
  },[expanded,activeIndex,listId]);
  const handleKey=event=>{
    onKeyDown?.(event);
    if(event.defaultPrevented||event.nativeEvent.isComposing) return;
    const available=matches.filter(item=>!item.disabled);
    if(['ArrowDown','ArrowUp'].includes(event.key)||(!editable&&['Home','End'].includes(event.key))) {
      event.preventDefault();setOpen(true);
      const index=available.findIndex(item=>item.value===active);
      const next=event.key==='Home'?0:event.key==='End'?available.length-1:event.key==='ArrowDown'?Math.min(index+1,available.length-1):index<=0?available.length-1:index-1;
      setActive(available[next]?.value??null);
    } else if(event.key==='Enter'&&expanded&&activeIndex>=0) {
      event.preventDefault();choose(matches[activeIndex]);
    } else if(!editable&&['Enter',' '].includes(event.key)) {
      event.preventDefault();setOpen(previous=>!previous);setActive(selected?.value??null);
    } else if(event.key==='Escape'&&expanded) {
      event.preventDefault();event.stopPropagation();close();
    } else if(event.key==='Tab') close();
    else if(!editable&&event.key.length===1&&!event.ctrlKey&&!event.metaKey&&!event.altKey) {
      const match=available.find(item=>String(item.label).toLocaleLowerCase().startsWith(event.key.toLocaleLowerCase()));
      if(match) {event.preventDefault();setOpen(true);setActive(match.value);}
    }
  };
  const accessibility={...props,id,disabled,role:'combobox','aria-expanded':expanded,'aria-controls':expanded?listId:undefined,'aria-haspopup':'listbox','aria-activedescendant':expanded&&activeIndex>=0?`${listId}-${activeIndex}`:undefined,'aria-required':required||undefined,onKeyDown:handleKey,onFocus,onBlur:event=>{if(!event.relatedTarget?.closest?.('.ui-popup')) close();onBlur?.(event);}};
  return <div ref={boundaryRef} className={`choice-control ${editable?'combobox':'select-control'}`}>
    {editable?<input {...accessibility} ref={triggerRef} className={`input ${className}`} name={name} form={form} required={required} value={text} placeholder={placeholder} aria-autocomplete="list" autoComplete={props.autoComplete||'off'} onFocus={event=>{setOpen(true);onFocus?.(event);}} onChange={event=>{setLocalValue(event.target.value);setOpen(true);setActive(null);onChange?.(event);}}/>
      :<><select ref={nativeRef} className="choice-native" tabIndex={-1} aria-hidden="true" name={name} form={form} disabled={disabled} required={required} value={text} onChange={event=>{setLocalValue(event.target.value);onChange?.(event);}} onInvalid={event=>{event.preventDefault();triggerRef.current?.focus();setOpen(true);}}>
        {!selected&&<option value="">{placeholder}</option>}
        {items.map(item=><option key={item.value} value={item.value} disabled={item.disabled}>{item.label}</option>)}
      </select><button {...accessibility} ref={triggerRef} type="button" className={`input choice-trigger ${className}`} onClick={()=>{setOpen(previous=>!previous);setActive(selected?.value??null);}}><span>{selected?.label||placeholder||'请选择'}</span><Icon name="chevron"/></button></>}
    {editable&&<button type="button" className="combobox-toggle" disabled={disabled} tabIndex={-1} aria-label="展开候选项" aria-expanded={expanded} onMouseDown={event=>event.preventDefault()} onClick={()=>{triggerRef.current?.focus();setOpen(!expanded);setActive(null);}}><Icon name="chevron"/></button>}
    <FloatingPanel anchorRef={triggerRef} boundaryRef={boundaryRef} open={expanded} onDismiss={close} id={listId} role="listbox" aria-label={props['aria-label']||'候选项'}>
      {matches.map((item,index)=><div key={item.value} id={`${listId}-${index}`} role="option" aria-selected={text===item.value} aria-disabled={item.disabled||undefined} className={`ui-popup-option ${active===item.value?'active':''}`} onMouseDown={event=>event.preventDefault()} onMouseEnter={()=>setActive(item.disabled?null:item.value)} onClick={()=>choose(item)}><span>{item.label}</span>{text===item.value&&<Icon name="check"/>}</div>)}
      {!matches.length&&<div className="ui-popup-empty">{editable?(text?'无匹配项，可使用当前输入':'暂无已有选项，可直接输入'):'暂无选项'}</div>}
    </FloatingPanel>
  </div>;
}

export const Select=props=><ChoiceControl {...props}/>;
export const ComboBox=props=><ChoiceControl {...props} editable/>;
