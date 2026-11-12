import{a as e,r as t}from"./i18n-YIZ94PuL.js";import{L as n,P as r,T as i,V as a,a as o,c as s,m as c,w as l}from"./router-DmWf1zNH.js";import{t as u}from"./live-D0VeMPic.js";import{a as d,i as f,n as p,r as m,t as h}from"./page-BvfONVkV.js";import{a as g,i as _,o as v}from"./controls-DMP3WW_j.js";var y=[{value:`daemon`,label:`daemon`},{value:`sidecar`,label:`sidecar`},{value:`audit`,get label(){return e(`d490f4d`)}}];function b(t,n){return t&&n?e(`dd884d6`,[c(t),c(n)]):t?e(`d6e1414`,[c(t)]):n?e(`df56398`,[c(n)]):``}function x(e,t=new Date){let n=new Date(e),r=e=>String(e).padStart(2,`0`),i=`${r(n.getHours())}:${r(n.getMinutes())}:${r(n.getSeconds())}`;return n.toDateString()===t.toDateString()?i:`${r(n.getMonth()+1)}-${r(n.getDate())} ${i}`}function S(e){return e.replace(/^(?:\[\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\] |(\[[A-Za-z]+\] )?\d{4}\/\d{2}\/\d{2} \d{2}:\d{2}:\d{2}(?:\.\d+)? )/,`$1`)}var C=class extends h{constructor(...e){super(...e),this.entries=[],this.query=``,this.errors=!1,this.loadingMore=!1,this.seq=0}get target(){return this.route}connectedCallback(){super.connectedCallback(),this.refresh()}disconnectedCallback(){super.disconnectedCallback(),clearTimeout(this.timer)}willUpdate(e){super.willUpdate(e);let t=e.get(`route`);t&&o(t)!==o(this.target)&&this.refresh()}params(e){let t=this.target;return{file:t.file??`daemon`,...e?{before:e}:{},...this.query.trim()?{query:this.query.trim()}:{},...t.agent?{agent:t.agent}:{},...t.since?{since:t.since}:{},...t.until?{until:t.until}:{},...this.errors?{errors:!0}:{}}}async refresh(){let e=++this.seq;try{let t=await this.call(`logs.tail`,this.params());if(e!==this.seq)return;this.entries=t.entries,this.cursor=t.before,this.loadError=``}catch(t){if(e!==this.seq)return;this.loadError=a(t)}this.loading=!1,this.requestUpdate()}async more(){if(!this.cursor)return;let e=this.seq;this.loadingMore=!0,this.requestUpdate();try{let t=await this.call(`logs.tail`,this.params(this.cursor));e===this.seq&&(this.entries=[...this.entries,...t.entries],this.cursor=t.before)}catch(e){this.loadError=a(e)}this.loadingMore=!1,this.requestUpdate()}go(e){let t=this.target,n={file:t.file,agent:t.agent,since:t.since,until:t.until,...e},r={page:`logs`};(n.file===`daemon`||n.file===`sidecar`||n.file===`audit`)&&(r.file=n.file),n.agent&&(r.agent=n.agent),n.since&&(r.since=n.since),n.until&&(r.until=n.until),s(o(r),{replace:!0})}toolbar(){let a=this.target,o=i.state.status?.agents??[],s=b(a.since,a.until);return n`<div class="toolbar log-tools">
      ${_({label:e(`df88d39`),size:`sm`,value:a.file??`daemon`,options:y,onChange:e=>this.go({file:e})})}
      <label class="search">
        ${l(`search`)}
        <input
          class="field-input"
          type="search"
          placeholder="${e(`d884e46`)}"
          aria-label="${e(`d0474b5`)}"
          .value=${u(this.query)}
          @input=${e=>{this.query=e.target.value,clearTimeout(this.timer),this.timer=setTimeout(()=>void this.refresh(),300)}}
        />
      </label>
      ${g({label:e(`dbbe7b2`),value:a.agent??``,options:[{value:``,label:e(`d198f2f`)},...o.map(e=>({value:e.alias,label:e.display||e.alias}))],onChange:e=>this.go({agent:e||void 0})})}
      <span class="log-errors">
        ${t(`d1d4d86`,[v({checked:this.errors,label:e(`dd40b5b`),onChange:e=>{this.errors=e,this.refresh()}})])}
      </span>
      ${s?n`<span class="chip">${s}</span>
            <button class="btn btn-sm btn-ghost" @click=${()=>this.go({since:void 0,until:void 0})}>${t(`de98833`)}</button>`:r}
    </div>`}row(e){return n`<div class="log-row ${e.error?`is-error`:``}">
      <span class="log-at">${e.at?x(e.at):``}</span>
      <pre class="log-text">${e.at?S(e.text):e.text}</pre>
    </div>`}render(){return n`<div class="page">
      ${f(e(`d7dbac1`),e(`d3e0845`))} ${this.toolbar()}
      ${this.loadError?m(e(`d6b62b0`),this.loadError,()=>this.refresh()):r}
      ${this.loading?d(6):this.entries.length?n`<div class="card log-list">${this.entries.map(e=>this.row(e))}</div>`:n`<div class="card">${p(e(`dda3a85`),this.cursor?e(`da05c61`):e(`d9a1ff7`))}</div>`}
      ${this.cursor?n`<div class="more-row">
            <button class="btn btn-sm" ?disabled=${this.loadingMore} @click=${()=>this.more()}>
              ${this.loadingMore?e(`d4927a5`):e(`dfab190`)}
            </button>
          </div>`:r}
    </div>`}};customElements.define(`sm-page-logs`,C);export{S as bodyText,x as logTime,b as rangeText};