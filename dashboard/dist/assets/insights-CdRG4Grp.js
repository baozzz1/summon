import{a as e,n as t,r as n,t as r}from"./i18n-YIZ94PuL.js";import{B as i,L as a,N as o,P as s,T as c,V as l,a as u,g as d,m as f,p,w as m,z as h}from"./router-DmWf1zNH.js";import{a as g,i as ee,n as _,r as v,t as y}from"./page-BvfONVkV.js";import{a as b}from"./controls-DMP3WW_j.js";import{r as x}from"./turn-FLcJ5dAu.js";import{n as te,t as ne}from"./date-range-DTnz-AAf.js";var S=8;function re(e,t){if(!(t>0))return s;let n=`${Math.min(100,Math.max(.5,t*100))}%`;return h`<svg class="ib-fill ib-${e}" width=${n} height=${S}>
    <rect width="100%" height=${S} rx="4"></rect><rect width="4" height=${S}></rect>
  </svg>`}function C(e,t){return a`<details class="uc-table">
    <summary>${n(`d9cee74`)}</summary>
    <div class="table-scroll">
      <table class="data-table" aria-label=${e}>
        <thead>
          <tr>${t.head.map((e,t)=>a`<th scope="col" class=${t?`num`:``}>${e}</th>`)}</tr>
        </thead>
        <tbody>
          ${t.rows.map(e=>a`<tr>${e.map((e,t)=>t?a`<td class="num">${e}</td>`:a`<th scope="row">${e}</th>`)}</tr>`)}
        </tbody>
      </table>
    </div>
  </details>`}function w(e){return a`<div class="uc-legend">
    ${e.map(e=>a`<span class="uc-legend-item"><span class="uc-swatch ib-key-${e.key}" aria-hidden="true"></span>${e.label}</span>`)}
  </div>`}var T=class extends o{constructor(...e){super(...e),this.locale=new r(this),this.hover=null}static{this.properties={series:{attribute:!1},rows:{attribute:!1},table:{attribute:!1},label:{},wide:{type:Boolean}}}createRenderRoot(){return this}setHover(e){this.hover!==e&&(this.hover=e,this.requestUpdate())}updated(){let e=this.querySelector(`.uc-tip`),t=this.hover===null?null:this.querySelectorAll(`.ib-row`)[this.hover];e&&t&&(e.style.top=`${t.offsetTop+t.offsetHeight+4}px`)}render(){let e=this.series??[],t=this.rows??[],n=this.hover===null?void 0:t[this.hover];return a`<div class="ib ${this.wide?`ib-wide`:``}">
      ${e.length>1?w(e):s}
      <div class="ib-rows" role="list" aria-label=${this.label} @pointerleave=${()=>this.setHover(null)}>
        ${t.map((e,t)=>a`<div class="ib-row ${this.hover!==null&&this.hover!==t?`is-dim`:``}" role="listitem" tabindex="0"
            aria-label=${e.tip.join(`，`)} data-key=${e.key}
            @pointerenter=${()=>this.setHover(t)} @focus=${()=>this.setHover(t)} @blur=${()=>this.setHover(null)}>
            <span class="ib-label">${e.label}</span>
            <span class="ib-bars">
              ${e.bars.map(e=>a`<span class="ib-bar"><svg class="ib-svg" width="100%" height=${S} aria-hidden="true">${re(e.series,e.fraction)}</svg><span class="ib-value">${e.text}</span></span>`)}
              ${e.note?a`<span class="ib-note">${e.note}</span>`:s}
            </span>
          </div>`)}
        ${n?a`<div class="uc-tip ib-tip" role="status">
              <div class="uc-tip-head">${n.tip[0]}</div>
              ${n.tip.slice(1).map(e=>a`<div class="ib-tip-line">${e}</div>`)}
            </div>`:s}
      </div>
      ${this.table?C(this.label,this.table):s}
    </div>`}};customElements.define(`sm-bar-list`,T);function E(e){return`${Number(e.slice(5,7))}/${Number(e.slice(8,10))}`}function ie(e){return e.weekday>=6}var D=160,O=8,k=32,ae=class extends o{constructor(...e){super(...e),this.locale=new r(this),this.width=640,this.hover=null}static{this.properties={days:{attribute:!1},dayTitle:{attribute:!1}}}createRenderRoot(){return this}connectedCallback(){super.connectedCallback(),this.observer=new ResizeObserver(e=>{let t=Math.round(e[0]?.contentRect.width??0);t>0&&t!==this.width&&(this.width=t,this.requestUpdate())}),this.observer.observe(this)}disconnectedCallback(){super.disconnectedCallback(),this.observer?.disconnect()}setHover(e){this.hover!==e&&(this.hover=e,this.requestUpdate())}updated(){let e=this.querySelector(`.uc-tip`);if(!e||this.hover===null)return;let t=(this.width-k)/Math.max(1,this.days.length),n=k+t*this.hover+t/2;e.style.left=`${Math.min(Math.max(n-e.offsetWidth/2,0),this.width-e.offsetWidth)}px`}label(t){return e(`d0f96ef`,[this.dayTitle(t),t.messages,t.chats])}render(){let t=this.days??[],r=Math.max(120,this.width-k),i=ne(Math.max(0,...t.map(e=>e.messages))),o=i[i.length-1],c=e=>168-e/o*D,l=r/Math.max(1,t.length),u=Math.max(2,Math.min(20,l-2)),d=Math.max(1,Math.ceil(48/l)),f=this.hover===null?void 0:t[this.hover];return a`<div class="uc">
      ${w([{key:`count`,label:e(`d07161a`)},{key:`weekend`,label:e(`d709b90`)}])}
      <div class="uc-plot" @pointerleave=${()=>this.setHover(null)}>
        <svg class="uc-svg" width=${this.width} height=${192} role="group" aria-label=${e(`db9893c`)}>
          ${i.map(e=>h`<line class="uc-grid" x1=${k} x2=${k+r} y1=${c(e)} y2=${c(e)}></line>
              <text class="uc-tick" x=${26} y=${c(e)} dy="0.32em" text-anchor="end">${e}</text>`)}
          ${t.map((e,t)=>{if(!e.messages)return s;let n=k+l*t+(l-u)/2,r=this.hover!==null&&this.hover!==t;return h`<path class="uc-col ib-day ${ie(e)?`ib-weekend`:`ib-count`} ${r?`is-dim`:``}" d=${te(n,c(e.messages),u,Math.max(1,c(0)-c(e.messages)))}></path>`})}
          ${t.map((e,n)=>(t.length-1-n)%d==0?h`<text class="uc-tick" x=${k+l*n+l/2} y=${184} text-anchor="middle">${E(e.date)}</text>`:s)}
          ${t.map((e,t)=>h`<rect class="uc-hit" x=${k+l*t} y=${O} width=${l} height=${D} tabindex="0" role="img" aria-label=${this.label(e)}
              @pointerenter=${()=>this.setHover(t)} @pointermove=${()=>this.setHover(t)}
              @focus=${()=>this.setHover(t)} @blur=${()=>this.setHover(null)}></rect>`)}
        </svg>
        ${f?a`<div class="uc-tip" role="status">
              <div class="uc-tip-head">${this.dayTitle(f)}</div>
              <div class="ib-tip-line">${n(`db514d4`,[f.messages])}</div>
              <div class="ib-tip-line">${n(`d39d036`,[f.chats])}</div>
            </div>`:s}
      </div>
      ${C(e(`db9893c`),{head:[e(`d70d0c1`),e(`d0d5e2b`),e(`d578c55`)],rows:t.map(e=>[this.dayTitle(e),String(e.messages),String(e.chats)])})}
    </div>`}};customElements.define(`sm-day-chart`,ae);var oe=/^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$/,se=/^\s*[-*+]\s+(.*)$/,ce=/^\s*(\d{1,9})[.)]\s+(.*)$/;function le(e){let t=[],n=[],r=null,i=()=>{n.length&&t.push({kind:`paragraph`,text:n.join(`
`)}),n=[],r=null};for(let a of e.replace(/\r\n?/g,`
`).split(`
`)){if(!a.trim()){i();continue}let e=oe.exec(a);if(e){i(),t.push({kind:`heading`,level:e[1].length<=2?3:4,text:e[2]});continue}let o=se.exec(a),s=o?null:ce.exec(a);if(o||s){let e=!!s;n.length&&i(),(!r||r.ordered!==e)&&(r={kind:`list`,ordered:e,start:s?Number(s[1]):1,items:[]},t.push(r)),r.items.push(o?o[1]:s[2])}else r&&/^\s/.test(a)?r.items[r.items.length-1]+=`\n${a.trim()}`:(r=null,n.push(a))}return i(),t}function A(e){let t=[],n=``,r=0,i=e=>{n&&t.push(n),n=``,t.push(e)};for(;r<e.length;){if(e[r]==="`"){let t=e.indexOf("`",r+1);if(t>r+1){i(a`<code>${e.slice(r+1,t)}</code>`),r=t+1;continue}}if(e.startsWith(`**`,r)){let t=e.indexOf(`**`,r+2);if(t>r+2){i(a`<strong>${A(e.slice(r+2,t))}</strong>`),r=t+2;continue}}n+=e[r],r+=1}return n&&t.push(n),t}function ue(e){switch(e.kind){case`heading`:return e.level===3?a`<h3>${A(e.text)}</h3>`:a`<h4>${A(e.text)}</h4>`;case`list`:return e.ordered?a`<ol start=${e.start}>${e.items.map(e=>a`<li>${A(e)}</li>`)}</ol>`:a`<ul>${e.items.map(e=>a`<li>${A(e)}</li>`)}</ul>`;case`paragraph`:return a`<p>${A(e.text)}</p>`}}function de(e){return a`<div class="analysis-text">${le(e).map(ue)}</div>`}var fe=3e3,pe={get configured(){return e(`analysis.source.configured`)},get usage(){return e(`analysis.source.usage`)},get first(){return e(`analysis.source.first`)}},me=u({page:`global`,mode:`form`,group:`insights`,key:`primary_agent`});function he(e){return c.state.status?.agents.find(t=>t.alias===e)?.display||e}function j(e,t=!1){return a`<p class="row-note analysis-note ${t?`is-warn`:``}">${m(t?`triangleAlert`:`info`,`icon-sm`)}<span>${e}</span></p>`}var M=class extends o{constructor(...e){super(...e),this.locale=new r(this),this.view=null,this.loadError=``,this.notice=``,this.starting=!1,this.generation=0,this.key=``,this.pending=null,this.call=(e,t)=>c.call(e,t),this.onVisibility=()=>{document.hidden?this.stopPolling():this.fetchView()}}static{this.properties={query:{attribute:!1}}}createRenderRoot(){return this}params(){return{...this.query,lang:t()}}connectedCallback(){super.connectedCallback(),document.addEventListener(`visibilitychange`,this.onVisibility),this.key=``,this.requestUpdate()}disconnectedCallback(){super.disconnectedCallback(),document.removeEventListener(`visibilitychange`,this.onVisibility),this.stopPolling(),this.generation+=1}willUpdate(){if(!this.query)return;let e=JSON.stringify(this.params());e!==this.key&&(this.key=e,this.generation+=1,this.view=null,this.loadError=``,this.notice=``,this.stopPolling(),this.fetchView())}stopPolling(){this.timer!==void 0&&clearTimeout(this.timer),this.timer=void 0}schedule(){this.stopPolling(),this.view?.status===`running`&&this.isConnected&&!document.hidden&&(this.timer=setTimeout(()=>{this.timer=void 0,this.fetchView()},fe))}async fetchView(){let e=this.generation;if(this.pending===e||!this.query)return;this.pending=e,this.stopPolling();let t=null,n=``;try{t=await this.call(`insights.analysis.get`,this.params())}catch(e){n=l(e)}this.pending===e&&(this.pending=null),e===this.generation&&this.isConnected&&(t&&(this.view=t),this.loadError=n,this.requestUpdate(),this.schedule())}async start(){if(this.starting||!this.view)return;let e=this.generation,t=this.view.status===`done`;this.starting=!0,this.notice=``,this.requestUpdate();try{let n=await this.call(`insights.analysis.start`,{...this.params(),...t?{force:!0}:{}});if(e!==this.generation)return;this.view=n,this.loadError=``}catch(t){if(e!==this.generation)return;this.notice=l(t),t instanceof i&&t.code===`forbidden`&&this.fetchView()}finally{this.starting=!1,this.requestUpdate()}this.schedule()}primaryLine(t){let n=t.primary;if(!n)return s;let r=[pe[n.source],n.cli,n.model].filter(Boolean).join(` · `);return a`<div class="analysis-primary">
      <span class="analysis-primary-text">
        <span class="muted">${e(`analysis.primary`)}</span>
        <strong>${n.label}</strong>
        <span class="muted">${r}</span>
      </span>
      <a class="link-btn" href=${me}>${e(`analysis.change`)}${m(`chevronRight`,`icon-sm`)}</a>
    </div>`}meta(t){let n=[e(`analysis.generatedAt`,[f(t.generated_at)]),he(t.agent),t.model,e(`analysis.elapsed`,[x(Math.round(t.elapsed_ms/1e3))]),t.usage?e(`analysis.tokens`,[p(t.usage.input),p(t.usage.output)]):void 0].filter(e=>!!e);return a`<p class="analysis-meta">${n.join(` · `)}</p>`}state(t){switch(t.status){case`none`:return a`<p class="analysis-empty">${e(`analysis.none`)}</p>`;case`running`:return a`<p class="analysis-running" role="status">
          ${m(`loaderCircle`,`icon-sm fs-spin`)}<span>${e(`analysis.running`,[t.started_at?d(t.started_at):`—`])}</span>
        </p>`;case`failed`:return j(e(`analysis.failed`,[t.reason??``]),!0);case`unsupported`:return j(t.reason??``,!0);case`done`:{let n=t.result;return n?a`${n.retry_failure?j(e(`analysis.retryFailed`,[n.retry_failure.reason]),!0):s}
          ${n.stale?j(e(`analysis.stale`)):s}
          ${de(n.text)}
          ${n.truncated?j(e(`analysis.truncated`)):s}
          ${this.meta(n)}`:s}}}actions(t){if(t.status===`running`||t.status===`unsupported`)return s;let n=t.status===`done`?e(`analysis.regenerate`):e(`analysis.generate`);return a`<div class="analysis-actions">
      <button class="btn ${t.status===`done`?``:`btn-primary`}" ?disabled=${!t.can_start||this.starting} @click=${()=>this.start()}>
        ${this.starting?m(`loaderCircle`,`icon-sm fs-spin`):m(`refreshCw`,`icon-sm`)}${n}
      </button>
      ${t.can_start?s:a`<span class="muted">${e(`analysis.denied`)}</span>`}
      ${this.notice?a`<span class="analysis-notice" role="alert">${m(`info`,`icon-sm`)}${this.notice}</span>`:s}
    </div>`}render(){let t=this.view;return a`<div class="group-head">
        <div>
          <h2 id="in-analysis">${e(`analysis.title`)}</h2>
          <p>${e(`analysis.subtitle`)}</p>
        </div>
      </div>
      ${t?a`<div class="card card-pad analysis" data-status=${t.status}>
            ${this.primaryLine(t)} ${this.state(t)}
            ${this.loadError?j(e(`analysis.refreshFailed`,[this.loadError]),!0):s}
            ${this.actions(t)}
          </div>`:this.loadError?v(e(`analysis.loadFailed`),this.loadError,()=>void this.fetchView()):g(2)}`}};customElements.define(`sm-insights-analysis`,M);var N=`summon-dash.insights.range`,P=`30d`;function F(){try{let e=localStorage.getItem(N);if(e===`relative`&&Number.isInteger(Number(localStorage.getItem(`${N}.hours`)))&&Number(localStorage.getItem(`${N}.hours`))>0||e===`today`||e===`7d`||e===`30d`||e===`90d`||e===`all`)return e}catch{}return P}function I(e){try{localStorage.setItem(N,e.range),e.range===`relative`&&localStorage.setItem(`${N}.hours`,String(e.hours))}catch{}}function L(e,t=1){return t>0&&e>0?x(e<1?0:e>=600?Math.round(e/60)*60:e):`—`}function R(e){return e>0?e<.01?`<1%`:`${Math.round(e*100)}%`:`0%`}function z(e){return(Math.round(e*10)/10).toFixed(1).replace(/\.0$/,``)}function B(t,n){if(!(n>0))return``;let r=(t-n)/n;return Math.abs(r)<.005?e(`da9105f`):`${r>0?`+`:`−`}${Math.round(Math.abs(r)*100)}%`}function V(e){return(e.previous?.gaps.n??0)>0}var H={get lt_1m(){return e(`d3dd879`)},get m1_5(){return e(`dfae951`)},get m5_15(){return e(`d88d8ed`)},get m15_60(){return e(`dd8c68a`)},get h1_4(){return e(`d2453b8`)},get gte_4h(){return e(`dd5a7de`)}},ge=[`m15_60`,`h1_4`,`gte_4h`],U={get instruction(){return e(`db2a33c`)},get short(){return e(`db0ec1f`)},get approve(){return e(`d561817`)},get poll(){return e(`dfe2371`)}},_e={get instruction(){return e(`dc5a06f`)},get short(){return e(`d6f453b`)},get approve(){return e(`d147221`)},get poll(){return e(`d5837ec`)}},W=(e,t)=>Number(e.params[t]??0);function G(t){switch(t.key){case`sample_small`:return e(`dc30a96`,[W(t,`n`)]);case`tail_concentration`:return e(`d698dbb`,[R(W(t,`count_share`)),R(W(t,`secs_share`))]);case`busy_elsewhere`:return e(`d0db839`,[R(W(t,`share`)),R(W(t,`silent_share`))]);case`mostly_instructions`:return e(`d191e9b`,[R(W(t,`approve_share`)),L(W(t,`approve_p50_secs`))]);case`polls`:return e(`d6bc98a`,[W(t,`count`),L(W(t,`p50_secs`))]);case`load`:return e(`da711a8`,[z(W(t,`messages_p50`)),z(W(t,`chats_p50`))]);case`human_vs_agent`:return e(`d57d7bd`,[z(W(t,`ratio`))]);case`trend`:return t.params.direction===`down`?e(`d35aef4`,[L(W(t,`previous_p50_secs`)),L(W(t,`p50_secs`))]):e(`db75e9f`,[L(W(t,`previous_p50_secs`)),L(W(t,`p50_secs`))])}}function K(e){let t=e.find(e=>e.key===`sample_small`);return t?[t]:e}function q(e){let t=e.buckets.filter(e=>ge.includes(e.key)),n=t.reduce((e,t)=>e+t.count,0),r=t.reduce((e,t)=>e+t.secs,0);return{count:e.gaps.n?n/e.gaps.n:0,secs:e.gaps.total_secs?r/e.gaps.total_secs:0}}function J(t){let n=t.gaps.n,r=t.gaps.total_secs,i=t.buckets.map(e=>({b:e,count:n?e.count/n:0,secs:r?e.secs/r:0})),a=Math.max(0,...i.flatMap(e=>[e.count,e.secs]))||1;return i.map(({b:t,count:n,secs:r})=>({key:t.key,label:H[t.key],note:e(`d233f28`,[p(t.count),L(t.secs,t.count)]),bars:[{series:`count`,fraction:n/a,text:R(n)},{series:`time`,fraction:r/a,text:R(r)}],tip:[H[t.key],e(`d4c3652`,[p(t.count),R(n)]),e(`d678404`,[L(t.secs,t.count),R(r)])]}))}function Y(t){return t.elsewhere.map(t=>{let n=t.count?t.active_elsewhere/t.count:0,r=e(`ddf0ee6`,[z(t.elsewhere_messages_p50)]);return{key:t.key,label:H[t.key],note:e(`d7b2430`,[p(t.count),r]),bars:[{series:`count`,fraction:n,text:t.count?R(n):`—`}],tip:[H[t.key],e(`df7ecdf`,[p(t.count),L(t.secs,t.count)]),e(`d0e7fb1`,[p(t.active_elsewhere),t.count?R(n):`—`]),r]}})}function X(t){let n=Math.max(0,...t.next_messages.map(e=>e.count))||1,r=t.next_messages.reduce((e,t)=>e+t.count,0);return t.next_messages.map(t=>{let i=e(`d45de09`,[L(t.p50_secs,t.count),L(t.p90_secs,t.count)]);return{key:t.key,label:U[t.key],note:i,bars:[{series:`count`,fraction:t.count/n,text:e(`dcba97a`,[p(t.count),r?R(t.count/r):`—`])}],tip:[`${U[t.key]} · ${_e[t.key]}`,e(`d70e7f1`,[p(t.count),r?R(t.count/r):`—`]),i]}})}function Z(n){let r=new Date(Date.UTC(2024,0,n.weekday)),i=new Intl.DateTimeFormat(t(),{weekday:t()===`en`?`short`:`narrow`,timeZone:`UTC`}).format(r);return e(`d119474`,[Number(n.date.slice(5,7)),Number(n.date.slice(8,10)),i])}function ve(e){return c.state.status?.agents.find(t=>t.alias===e)?.display||e}var Q=[{key:`count`,get label(){return e(`da4dcf6`)}}],ye=[{key:`count`,get label(){return e(`da4dcf6`)}},{key:`time`,get label(){return e(`d90e693`)}}],$=class extends y{constructor(...e){super(...e),this.range=F(),this.hours=(()=>{try{return Number(localStorage.getItem(`${N}.hours`))||24}catch{return 24}})(),this.agent=``,this.requestId=0,this.view=null,this.refetching=!1}connectedCallback(){super.connectedCallback(),this.refresh()}query(){return{range:this.range,...this.range===`relative`?{hours:this.hours}:{},...this.range===`custom`?{from:this.from,to:this.to}:{},...this.agent?{agent:this.agent}:{}}}async refresh(){let e=++this.requestId;this.refetching=this.view!==null,this.requestUpdate();let t=null,n=``;try{t=await this.call(`insights.summary`,this.query())}catch(e){n=l(e)}e===this.requestId&&(this.view=t,this.loadError=n,this.loading=!1,this.refetching=!1,this.requestUpdate())}setRange(e){this.range=e.range,this.from=e.from,this.to=e.to,this.hours=e.hours??24,e.range!==`custom`&&I(e),this.refresh()}updated(){let e=this.querySelector(`[data-control=insights-agent]`);e&&(e.value=this.agent)}filters(){let t=new Map((c.state.status?.agents??[]).map(e=>[e.alias,e.display||e.alias]));this.agent&&!t.has(this.agent)&&t.set(this.agent,ve(this.agent));let r=this.view;return a`<div class="usage-filters">
      <label>Agent ${b({id:`insights-agent`,label:e(`dbefab7`),value:this.agent,options:[{value:``,label:e(`d198f2f`)},...[...t].map(([e,t])=>({value:e,label:t}))],onChange:e=>{this.agent=e,this.refresh()}})}</label>
      ${r?a`<span class="muted">${r.range===`relative`?n(`range.executedTimes`,[r.from.replace(`T`,` `),r.to.replace(`T`,` `)]):n(`d5db313`,[r.from,r.to])}</span>`:s}
    </div>`}findings(t){let r=K(t.findings);return a`<section class="page-section" aria-labelledby="in-findings">
      <div class="group-head">
        <div>
          <h2 id="in-findings">${n(`d14163a`)}</h2>
          <p>${n(`d63f5fb`)}</p>
        </div>
      </div>
      <div class="card">
        ${r.length?a`<ul class="finding-list">
              ${r.map(e=>a`<li class="finding ${e.key===`sample_small`?`is-notice`:``}" data-key=${e.key}>${m(e.key===`sample_small`?`info`:`lightbulb`,`icon-sm`)}<span>${G(e)}</span></li>`)}
            </ul>`:_(e(`d243cb8`),e(`d9da773`))}
      </div>
    </section>`}stat(e,t,n){return a`<div class="stat">
      <div class="stat-label">${e}</div>
      <div class="stat-value stat-figure">${t}</div>
      ${n?a`<div class="stat-sub">${n}</div>`:s}
    </div>`}versus(t,n,r){if(n===void 0)return;let i=B(t,n);return i?e(`d284b22`,[r,i]):e(`dc3d5ef`,[r])}overview(t){let r=t.gaps,i=V(t)?t.previous.gaps:void 0,o=t.agent_turns,s=o.total_secs>0&&r.n>0?e(`db50a49`,[z(r.total_secs/o.total_secs)]):`—`,c=e=>this.versus(r[e],i?.[e],L(i?.[e]??0,i?.n));return a`<section class="page-section" aria-labelledby="in-overview">
      <div class="group-head">
        <div>
          <h2 id="in-overview">${n(`dfea405`)}</h2>
          <p>${n(`d5ce21a`,[p(t.human_turns),t.agents,t.chats])}</p>
        </div>
      </div>
      <div class="card">
        <div class="stats stats-insights">
          ${this.stat(e(`dbd9f05`),p(r.n),this.versus(r.n,i?.n,e(`d5cc275`,[p(i?.n??0)])))}
          ${this.stat(e(`d5034a4`),L(r.p50_secs,r.n),c(`p50_secs`))}
          ${this.stat(`P75`,L(r.p75_secs,r.n),c(`p75_secs`))}
          ${this.stat(`P90`,L(r.p90_secs,r.n),c(`p90_secs`))}
          ${this.stat(e(`d793403`),L(r.total_secs,r.n),c(`total_secs`))}
          ${this.stat(e(`d29ae0a`),L(o.p50_secs,o.n),e(`d3dca46`,[L(o.p90_secs,o.n),p(o.n)]))}
          ${this.stat(e(`d37c1fb`),L(o.total_secs,o.n))}
          ${this.stat(e(`d520715`),s)}
        </div>
      </div>
    </section>`}distribution(t){let r=q(t),i=V(t)?t.previous:void 0;return a`<section class="page-section" aria-labelledby="in-dist">
      <div class="group-head">
        <div>
          <h2 id="in-dist">${n(`d89faa5`)}</h2>
          <p>${t.gaps.n?n(`d8f092c`,[R(r.count),R(r.secs)]):s}${t.gaps.n&&i?n(`d26612d`,[R(i.over_15m_count_share),R(i.over_15m_secs_share)]):s}</p>
        </div>
      </div>
      <div class="card card-pad">
        ${t.gaps.n?a`<sm-bar-list .series=${ye} .rows=${J(t)} label=${e(`ddad469`)}
              .table=${{head:[e(`d4f5df7`),e(`de18aa1`),e(`da4dcf6`),e(`d4fb147`),e(`d90e693`)],rows:t.buckets.map(e=>[H[e.key],p(e.count),R(t.gaps.n?e.count/t.gaps.n:0),L(e.secs,e.count),R(t.gaps.total_secs?e.secs/t.gaps.total_secs:0)])}}></sm-bar-list>`:this.noGaps()}
      </div>
    </section>`}noGaps(){return _(e(`d51af1a`),e(`d72a198`))}elsewhere(t){return a`<section class="page-section" aria-labelledby="in-elsewhere">
      <div class="group-head">
        <div>
          <h2 id="in-elsewhere">${n(`dd7bcab`)}</h2>
          <p>${n(`df832ac`)}</p>
        </div>
      </div>
      <div class="card card-pad">
        ${t.long_gaps?a`<sm-bar-list .series=${Q} .rows=${Y(t)} label=${e(`d4759f6`)}
                .table=${{head:[e(`d4f5df7`),e(`de18aa1`),e(`dac5c50`),e(`d11147c`)],rows:t.elsewhere.map(t=>[H[t.key],p(t.count),e(`d73d555`,[p(t.active_elsewhere),t.count?R(t.active_elsewhere/t.count):`—`]),z(t.elsewhere_messages_p50)])}}></sm-bar-list>
              <p class="row-note">${m(`info`,`icon-sm`)}<span>${n(`d895924`,[p(t.long_gaps),p(t.long_gaps_silent),R(t.long_gaps_silent/t.long_gaps)])}</span></p>`:t.gaps.n?_(e(`d25c89e`),``):this.noGaps()}
      </div>
    </section>`}nextMessages(t){return a`<section class="page-section" aria-labelledby="in-next">
      <div class="group-head">
        <div>
          <h2 id="in-next">${n(`d9e879c`)}</h2>
          <p>${n(`d1f8684`)}</p>
        </div>
      </div>
      <div class="card card-pad">
        ${t.gaps.n?a`<sm-bar-list wide .series=${Q} .rows=${X(t)} label=${e(`d1e488f`)}
              .table=${{head:[e(`dba4001`),e(`de18aa1`),e(`d5034a4`),`P90`],rows:t.next_messages.map(e=>[U[e.key],p(e.count),L(e.p50_secs,e.count),L(e.p90_secs,e.count)])}}></sm-bar-list>`:this.noGaps()}
      </div>
    </section>`}daily(t){let r=t.workdays;return a`<section class="page-section" aria-labelledby="in-daily">
      <div class="group-head">
        <div>
          <h2 id="in-daily">${n(`dfbac42`)}</h2>
          <p>${r.days?n(`d8dbc74`,[r.days,z(r.messages_p50),z(r.messages_p90),z(r.chats_p50),z(r.chats_p90)]):n(`d2bdb32`)}</p>
        </div>
      </div>
      <div class="card card-pad">
        ${t.days.some(e=>e.messages>0)?a`<sm-day-chart .days=${t.days} .dayTitle=${Z}></sm-day-chart>`:_(e(`d2f780d`),``)}
      </div>
    </section>`}caliber(e){let t=[String(e.day_start_hour).padStart(2,`0`),String(e.day_end_hour).padStart(2,`0`)];return a`<section class="page-section" aria-labelledby="in-caliber">
      <div class="group-head">
        <div><h2 id="in-caliber">${n(`d380869`)}</h2></div>
      </div>
      <div class="card card-pad">
        <ul class="caliber-list">
          <li>${n(`d1f2cb0`)}</li>
          <li>${n(`da42c2a`,[t[0],t[1],p(e.gaps_including_overnight)])}</li>
          <li>${n(`da2a731`,[p(e.queued)])}</li>
          <li>${n(`d0fa3f6`)}</li>
          <li>${n(`de22c64`)}</li>
          <li>${n(`de853fe`)}</li>
          ${e.range===`today`&&e.previous?a`<li>${n(`df07340`)}</li>`:s}
        </ul>
      </div>
    </section>`}body(t){return t.human_turns?a`${this.findings(t)} <sm-insights-analysis class="page-section" role="region" aria-labelledby="in-analysis" .query=${this.query()}></sm-insights-analysis> ${this.overview(t)} ${this.distribution(t)} ${this.elsewhere(t)} ${this.nextMessages(t)} ${this.daily(t)}
      ${this.caliber(t)}`:a`<section class="page-section"><div class="card">
          ${_(e(`d2f780d`),e(`d66e686`))}
        </div></section>
        ${this.caliber(t)}`}render(){let t=this.view,r=t&&this.range!==`custom`?t.to.slice(0,10):new Date().toLocaleDateString(`en-CA`);return a`<div class="page">
      ${ee(e(`d9d7407`),e(`d6f6c60`))}
      <div class="usage-toolbar" role="region" aria-label="${e(`d45c828`)}">
        <span class="usage-toolbar-label">${n(`dc95ff6`)}</span>
        <sm-date-range .query=${{range:this.range,from:this.from,to:this.to,hours:this.hours}} .today=${r} .since=${t?.from??``}
          @range-change=${e=>this.setRange(e.detail)}></sm-date-range>
      </div>
      ${this.filters()}
      ${this.loadError?v(e(`d9e90fa`),this.loadError,()=>this.refresh()):s}
      ${this.loading?g(4):t?a`<div class="usage ${this.refetching?`is-stale`:``}">${this.body(t)}</div>`:s}
    </div>`}};customElements.define(`sm-page-insights`,$);export{$ as InsightsPage,U as NEXT_LABEL,H as SPAN_LABEL,J as bucketRows,B as change,V as comparable,Z as dayTitle,Y as elsewhereRows,G as findingText,q as longShares,X as nextRows,R as percent,z as ratio,K as shownFindings,L as span};