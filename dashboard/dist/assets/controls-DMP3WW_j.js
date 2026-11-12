import{a as e,r as t,t as n}from"./i18n-YIZ94PuL.js";import{C as r,L as i,N as a,P as o,h as s,u as c,w as l,x as u}from"./router-DmWf1zNH.js";import{t as d}from"./live-D0VeMPic.js";import{n as f,t as p}from"./ident-Bw3GTApD.js";function m(e){return i`<button
    type="button"
    class="switch"
    role="switch"
    aria-checked=${e.checked?`true`:`false`}
    aria-label=${e.label}
    ?disabled=${e.disabled}
    @click=${()=>e.onChange(!e.checked)}
  >
    <span class="switch-track"></span>
  </button>`}var h=0;function g(e){let t=`seg-${++h}`;return i`<div class="seg ${e.size===`sm`?`seg-sm`:``}" role="radiogroup" aria-label=${e.label}>
    ${e.options.map(n=>i`<label class="seg-opt">
        <input
          type="radio"
          name=${t}
          value=${n.value}
          .checked=${d(e.value===n.value)}
          ?disabled=${e.disabled}
          @change=${()=>e.onChange(n.value)}
        /><span>${n.label}</span>
      </label>`)}
  </div>`}function _(t){let n=t.options.some(e=>e.value===t.value);return i`<span class="select-wrap">
    <select
      class="select"
      data-control=${t.id??o}
      aria-label=${t.label}
      ?disabled=${t.disabled}
      .value=${d(t.value??``)}
      @change=${e=>t.onChange(e.target.value)}
    >
      ${n?o:i`<option value=${t.value??``}>${t.value||e(`d2f5f1d`)}</option>`}
      ${t.options.map(e=>i`<option value=${e.value} ?selected=${e.value===t.value}>${e.label}</option>`)}
    </select>
    ${l(`chevronDown`)}
  </span>`}function v(e){return i`<input
    class="field-input ${e.mono?`mono`:``} ${e.wide?`wide`:``}"
    type="text"
    spellcheck="false"
    autocomplete="off"
    aria-label=${e.label}
    placeholder=${e.placeholder??``}
    list=${e.list??o}
    ?disabled=${e.disabled}
    .value=${d(e.value)}
    @input=${t=>e.onChange(t.target.value)}
  />`}function y(e){return i`<input
    class="field-input num"
    type="number"
    inputmode="numeric"
    aria-label=${e.label}
    min=${e.min??o}
    max=${e.max??o}
    ?disabled=${e.disabled}
    .value=${d(e.value===null?``:String(e.value))}
    @input=${t=>{let n=t.target.value;e.onChange(n===``?null:Number(n))}}
  />`}function b(t){let n=typeof t.value==`number`?t.value:0,{amount:a,unit:s}=r(n);return i`<span class="dur">
    <input
      class="field-input num"
      type="number"
      inputmode="numeric"
      min=${Math.ceil((t.minSecs??0)/s)}
      aria-label=${e(`d4e2271`,[t.label])}
      placeholder=${t.minSecs?o:e(`d5d0ae6`)}
      ?disabled=${t.disabled}
      .value=${d(n>0?String(a):``)}
      @input=${e=>{let n=e.target.value,r=n===``?0:Math.max(0,Math.round(Number(n)));t.onChange(r*s)}}
    />
    <span class="select-wrap">
      <select
        class="select"
        aria-label=${e(`db12052`,[t.label])}
        ?disabled=${t.disabled}
        .value=${d(String(s))}
        @change=${e=>t.onChange((n>0?a:1)*Number(e.target.value))}
      >
        ${c.map(e=>i`<option value=${String(e.secs)} ?selected=${e.secs===s}>${e.label}</option>`)}
      </select>
      ${l(`chevronDown`)}
    </span>
  </span>`}function x(e){return i`<span class="dur-human">${s(e)}</span>`}var S=class extends a{static{this.properties={values:{attribute:!1},members:{attribute:!1},disabled:{type:Boolean},label:{type:String},placeholder:{type:String}}}constructor(){super(),this.locale=new n(this),this.values=[],this.members=[],this.disabled=!1,this.label=``,this.placeholder=e(`d39d59e`)}createRenderRoot(){return this}emit(e){this.dispatchEvent(new CustomEvent(`change`,{detail:e,bubbles:!0}))}add(e){let t=e.split(/[\s,，、]+/).map(e=>e.trim()).filter(Boolean).filter(e=>!this.values.includes(e));return t.length?(this.emit([...this.values,...t]),!0):!1}onKey(e){let t=e.target;(e.key===`Enter`||e.key===`,`)&&t.value.trim()?(e.preventDefault(),this.add(t.value)&&(t.value=``)):e.key===`Backspace`&&!t.value&&this.values.length&&this.emit(this.values.slice(0,-1))}onBlur(e){let t=e.target;t.value.trim()&&this.add(t.value)&&(t.value=``)}render(){let n=new Map(this.members.map(e=>[e.id,e.name]));return i`<div
      class="tags"
      aria-disabled=${this.disabled?`true`:`false`}
      @click=${e=>{e.target===e.currentTarget&&this.querySelector(`.tags-input`)?.focus()}}
    >
      ${this.values.map(t=>{let r=n.get(t);return i`<span class="tag" title=${t}>
          ${f(t)?p(t,{name:r??``}):i`<span class="tag-id">${t}</span>`}
          ${this.disabled?o:i`<button
                type="button"
                class="tag-x"
                aria-label=${e(`da32111`,[r??t])}
                @click=${()=>this.emit(this.values.filter(e=>e!==t))}
              >
                ${l(`x`,`icon-sm`)}
              </button>`}
        </span>`})}
      ${this.disabled?this.values.length?o:i`<span class="tag-id">${t(`d2f8267`)}</span>`:i`<input
            class="tags-input"
            type="text"
            spellcheck="false"
            autocomplete="off"
            aria-label=${this.label}
            placeholder=${this.values.length?``:this.placeholder}
            @keydown=${this.onKey}
            @blur=${this.onBlur}
          />`}
    </div>`}};customElements.define(`sm-tags`,S);var C=`«summon:redacted»`,w=class extends a{static{this.properties={value:{attribute:!1},disabled:{type:Boolean},sensitive:{type:Boolean},label:{type:String},keyPlaceholder:{type:String},valuePlaceholder:{type:String}}}constructor(){super(),this.locale=new n(this),this.rows=[],this.lastEmitted=null,this.value={},this.disabled=!1,this.sensitive=!1,this.label=``,this.keyPlaceholder=e(`d721b00`),this.valuePlaceholder=e(`dcda1d5`)}createRenderRoot(){return this}willUpdate(e){e.has(`value`)&&!u(this.value,this.lastEmitted)&&(this.rows=Object.entries(this.value??{}).map(([e,t])=>({k:e,v:String(t)})))}emit(){let e={};for(let t of this.rows)t.k.trim()&&(e[t.k.trim()]=t.v);this.lastEmitted=e,this.dispatchEvent(new CustomEvent(`change`,{detail:e,bubbles:!0}))}edit(e,t){this.rows=this.rows.map((n,r)=>r===e?{...n,...t}:n),this.emit(),this.requestUpdate()}render(){return i`<div class="kv" role="group" aria-label=${this.label}>
      ${this.rows.map((t,n)=>{let r=this.sensitive&&t.v===`«summon:redacted»`;return i`<div class="kv-row">
          <div class="kv-key">
            <input
              class="field-input mono"
              type="text"
              spellcheck="false"
              autocomplete="off"
              aria-label=${e(`d5d2bce`,[n+1])}
              placeholder=${this.keyPlaceholder}
              ?disabled=${this.disabled}
              .value=${d(t.k)}
              @input=${e=>this.edit(n,{k:e.target.value})}
            />
            ${f(t.k.trim())?i`<span class="kv-ident">${p(t.k.trim())}</span>`:o}
          </div>
          <input
            class="field-input mono kv-val"
            type="text"
            spellcheck="false"
            autocomplete="off"
            aria-label=${e(`d5cb36c`,[n+1])}
            placeholder=${r?e(`da8c392`):this.valuePlaceholder}
            ?disabled=${this.disabled}
            .value=${d(r?``:t.v)}
            @input=${e=>{let r=e.target.value;this.edit(n,{v:this.sensitive&&r===``&&this.value[t.k]===`«summon:redacted»`?C:r})}}
          />
          ${this.disabled?o:i`<button
                type="button"
                class="icon-btn"
                aria-label=${e(`d94c3a4`,[t.k||e(`d5fb0f5`,[n+1])])}
                @click=${()=>{this.rows=this.rows.filter((e,t)=>t!==n),this.emit(),this.requestUpdate()}}
              >
                ${l(`x`)}
              </button>`}
        </div>`})}
      ${this.disabled?this.rows.length?o:i`<span class="muted">${t(`d2f8267`)}</span>`:i`<button
            type="button"
            class="btn btn-sm btn-ghost kv-add"
            @click=${()=>{this.rows=[...this.rows,{k:``,v:``}],this.requestUpdate(),this.updateComplete.then(()=>{let e=this.querySelectorAll(`.kv-row input`);e[e.length-2]?.focus()})}}
          >
            ${t(`d4b67ac`,[l(`plus`,`icon-sm`)])}
          </button>`}
    </div>`}};customElements.define(`sm-kv`,w);export{_ as a,g as i,x as n,m as o,y as r,v as s,b as t};