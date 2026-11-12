import{a as e,r as t}from"./i18n-YIZ94PuL.js";import{L as n,P as r,T as i,V as a,a as o,w as s}from"./router-DmWf1zNH.js";import{t as c}from"./live-D0VeMPic.js";import{a as l,i as u,n as d,r as f,t as p}from"./page-BvfONVkV.js";import{i as m}from"./controls-DMP3WW_j.js";import{n as h,r as g,t as _}from"./schedule-DKJtnJT_.js";var v=6,y=[{value:`agent`,label:`Agent`},{value:`chat`,get label(){return e(`d35b49e`)}},{value:`kind`,get label(){return e(`d5c428f`)}}],b=`summon-dash.schedules.group-by`;function x(){try{let e=localStorage.getItem(b);if(e===`agent`||e===`chat`||e===`kind`)return e}catch{}return`agent`}function S(e){try{localStorage.setItem(b,e)}catch{}}var C=[{key:`once`,get title(){return e(`d3de42d`)},kinds:[`once`]},{key:`repeat`,get title(){return e(`dad3221`)},kinds:[`interval`,`cron`]}];function w(e,t,n){let r=e=>e.chat_name??e.chat_id,i=(t,n,r,i)=>{let a=new Map;for(let o of e){let e=a.get(t(o))??{...r(o),subs:[]};a.set(t(o),e);let s=e.subs.find(e=>e.key===n(o));s||e.subs.push(s={...i(o),items:[]}),s.items.push(o)}return[...a.values()]};switch(t){case`agent`:return i(e=>e.agent,e=>e.chat_id,e=>({key:e.agent,title:n(e.agent),aside:e.agent,avatar:n(e.agent)}),e=>({key:e.chat_id,title:r(e),href:o({page:`chat`,chatId:e.chat_id,agent:e.agent})}));case`chat`:return i(e=>e.chat_id,e=>e.agent,e=>({key:e.chat_id,title:r(e),avatar:r(e)}),e=>({key:e.agent,title:n(e.agent),href:o({page:`chat`,chatId:e.chat_id,agent:e.agent})}));case`kind`:return C.map(t=>({key:t.key,title:t.title,avatar:t.title,subs:[{key:t.key,title:``,items:e.filter(e=>t.kinds.includes(e.kind))}]})).filter(e=>e.subs[0].items.length)}}var T=class extends p{constructor(...e){super(...e),this.list=[],this.query=``,this.page=0,this.busy=new Set,this.runs=new Map,this.groupBy=x()}connectedCallback(){super.connectedCallback(),this.refresh()}watchKeys(){return[`schedules`]}async refresh(){try{this.list=await this.call(`schedules.list`,{}),this.loadError=``}catch(e){this.loadError=a(e)}this.loading=!1,this.requestUpdate()}replace(e){this.list=this.list.map(t=>t.id===e.id?e:t),this.requestUpdate()}async toggle(e,t){this.busy.add(e.id),this.requestUpdate();let n=await g(this.call,e,t);this.busy.delete(e.id),n?this.replace(n):this.requestUpdate(),await i.refreshStatus(!0)}async edit(e){let t=await _(this.call,e);t&&(this.replace(t),await i.refreshStatus(!0))}async toggleRuns(e,t){let n=this.runs.get(e.id),r=t&&!n?.turns;if(this.runs.set(e.id,{open:t,loading:r,...n?.turns?{turns:n.turns}:{}}),this.requestUpdate(),!r)return;let i=()=>this.runs.get(e.id)?.open??t;try{let t=await this.call(`turns.list`,{agent:e.agent,schedule:e.id,limit:10});this.runs.set(e.id,{open:i(),loading:!1,turns:t.turns})}catch(t){this.runs.set(e.id,{open:i(),loading:!1,error:a(t)})}this.requestUpdate()}display(e){return i.state.status?.agents.find(t=>t.alias===e)?.display||e}rows(e,t=!1,r=!1){return n`${e.map(e=>h(e,{busy:this.busy.has(e.id),showChat:t,...this.runs.has(e.id)?{runs:this.runs.get(e.id)}:{},onRuns:(e,t)=>void this.toggleRuns(e,t),...r?{agentLabel:this.display(e.agent)}:{},onToggle:(e,t)=>this.toggle(e,t),onEdit:e=>this.edit(e)}))}`}active(){let i=this.list.filter(e=>e.enabled);if(!i.length)return n`<div class="card">${d(e(`dc4869e`),e(`d40bcc0`))}</div>`;let a=this.groupBy===`kind`;return n`${w(i,this.groupBy,e=>this.display(e)).map(e=>n`<section class="sched-group" aria-labelledby=${`sg-${e.key}`}>
        <header class="sched-group-head">
          <span class="avatar" aria-hidden="true">${[...e.avatar][0]}</span>
          <h2 id=${`sg-${e.key}`}>${e.title}${e.aside?n` <span class="mono muted">${e.aside}</span>`:r}</h2>
          ${a?n`<span class="muted count">${t(`d376dc4`,[e.subs[0].items.length])}</span>`:r}
        </header>
        ${e.subs.map(e=>a?n`<div class="card">${this.rows(e.items,!0,!0)}</div>`:n`<div class="sched-chat">
                <div class="sched-chat-head">
                  <a class="link-btn quiet" href=${e.href??`#`}>${e.title}</a>
                  <span class="muted count">${t(`d376dc4`,[e.items.length])}</span>
                </div>
                <div class="card">${this.rows(e.items)}</div>
              </div>`)}
      </section>`)}`}archived(){let i=this.list.filter(e=>!e.enabled);if(!i.length)return r;let a=this.query.trim().toLowerCase(),o=a?i.filter(e=>[e.id,e.prompt,e.human,e.chat_name??``,e.agent,this.display(e.agent)].join(` `).toLowerCase().includes(a)):i,l=Math.max(1,Math.ceil(o.length/v)),u=Math.min(this.page,l-1),f=o.slice(u*v,u*v+v);return n`<details class="archive">
      <summary>${t(`d672be4`,[s(`chevronRight`,`icon-chev`)])} <span class="muted count">${t(`d376dc4`,[i.length])}</span></summary>
      <div class="archive-body">
        <p class="muted archive-note">${t(`dab8dbc`)}</p>
        <div class="toolbar archive-tools">
          <label class="search">
            ${s(`search`)}
            <input
              class="field-input"
              type="search"
              placeholder="${e(`d8b7e6e`)}"
              aria-label="${e(`d89c67b`)}"
              .value=${c(this.query)}
              @input=${e=>{this.query=e.target.value,this.page=0,this.requestUpdate()}}
            />
          </label>
        </div>
        ${o.length?n`<div class="card">
              ${this.rows(f,!0)}
              ${l>1?n`<div class="pager">
                    <span class="tnum">${t(`d4182cb`,[u+1,l,o.length])}</span>
                    <div class="pager-btns">
                      <button class="btn btn-sm" ?disabled=${u===0} @click=${()=>this.setPage(u-1)} aria-label="${e(`dc9b9ae`)}">
                        ${s(`chevronLeft`,`icon-sm`)}
                      </button>
                      <button class="btn btn-sm" ?disabled=${u>=l-1} @click=${()=>this.setPage(u+1)} aria-label="${e(`d8a8542`)}">
                        ${s(`chevronRight`,`icon-sm`)}
                      </button>
                    </div>
                  </div>`:r}
            </div>`:n`<div class="card">${d(e(`d7791c0`),e(`d5a7c1c`))}</div>`}
      </div>
    </details>`}setPage(e){this.page=e,this.requestUpdate()}render(){return n`<div class="page">
      ${u(e(`d3c39e9`),e(`d48250b`),m({label:e(`d72148c`),size:`sm`,value:this.groupBy,options:y,onChange:e=>{this.groupBy=e,S(this.groupBy),this.requestUpdate()}}))}
      ${this.loadError?f(e(`d494c50`),this.loadError,()=>this.refresh()):r}
      ${this.loading?l(4):n`${this.active()}${this.archived()}`}
    </div>`}};customElements.define(`sm-page-schedules`,T);export{w as groupSchedules};