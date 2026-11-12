import{a as e,r as t}from"./i18n-YIZ94PuL.js";import{B as n,L as r,P as i,T as a,V as o,w as s}from"./router-DmWf1zNH.js";import{i as c}from"./labels-DeJm4sRU.js";import{a as l,c as u,d,f,h as p,i as m,o as h,p as g,r as _,t as v}from"./page-BvfONVkV.js";import{t as y}from"./ident-Bw3GTApD.js";import{o as b,s as x}from"./controls-DMP3WW_j.js";import{n as S,t as C}from"./panel-oHAd-YCH.js";import{t as w}from"./field-row-CbIZQm7V.js";import{n as T,r as E,t as D}from"./schedule-DKJtnJT_.js";var O={get chat_override(){return e(`d16973c`)},get chat_mapping(){return e(`dacbb8c`)},get agent_default(){return e(`d968ebe`)}},k=class extends v{constructor(...e){super(...e),this.chatId=``,this.detail=null,this.sections=new Map,this.fields=new Map,this.active=``,this.preview=new C(this.call,null,()=>this.sections.get(this.active)?.config.changeList()??[]),this.scrolled=!1}languageChanged(){this.fields=new Map((a.state.schema?.fields??[]).map(e=>[e.key,e])),this.preview&&this.preview.request(),super.languageChanged()}connectedCallback(){super.connectedCallback();for(let e of a.state.schema?.fields??[])this.fields.set(e.key,e);this.bindPreview(this.preview)}disconnectedCallback(){super.disconnectedCallback(),this.preview.dispose()}willUpdate(){let e=this.route;e&&e.chatId!==this.chatId&&(this.chatId=e.chatId,this.detail=null,this.sections.clear(),this.loading=!0,this.scrolled=!1,this.refresh())}watchKeys(){return[`schedules`]}agentName(e){return a.state.status?.agents.find(t=>t.alias===e)?.display||e}build(t){let n=this.chatId,r={level:`chat`,agent:t.agent,chat_id:n},i=this.agentName(t.agent),a=new f(r,()=>e(`dbc3dde`,[i]),()=>this.fields,this.call,()=>{this.active===t.agent&&this.preview.schedule(),this.requestUpdate()});a.setView(t.config);let o={view:t,config:a,opRevision:t.operators.revision,operators:new p(`ops:${t.agent}`,()=>e(`d2a59d4`,[i]),t.operators.members.map(e=>e.id),async e=>{let r=await this.call(`chats.set_operators`,{agent:t.agent,chat_id:n,revision:o.opRevision,members:e});return o.opRevision=r.revision,e},async()=>(await this.refetch(o))?.operators.members.map(e=>e.id)??[],()=>this.requestUpdate(),`agent:${t.agent}`),mapping:new p(`wd:${t.agent}`,()=>e(`dc994af`,[i]),t.work_dir.mapping??``,async e=>{let r=await this.call(`chats.set_work_dir`,{agent:t.agent,chat_id:n,revision:o.opRevision,mapping:e.trim()||null,clear_override:!1});return o.view=r,o.opRevision=r.operators.revision,r.work_dir.mapping??``},async()=>(await this.refetch(o))?.work_dir.mapping??``,()=>this.requestUpdate(),`agent:${t.agent}`),doc:new g({level:`chat`,agent:t.agent,chat_id:n},()=>e(`dc27f3c`,[i]),this.call,()=>this.requestUpdate()),docOpen:t.doc.exists,job:null,busy:!1};return o}async refetch(e){let t=(await this.call(`chats.get`,{chat_id:this.chatId})).agents.find(t=>t.agent===e.view.agent);return t&&(e.view=t,e.opRevision=t.operators.revision,e.config.setView(t.config)),t}async refresh(){let e=this.chatId;try{let t=await this.call(`chats.get`,{chat_id:e});if(e!==this.chatId)return;this.detail=t;for(let e of t.agents){let t=this.sections.get(e.agent);t?(t.view=e,t.config.setView(e.config),t.opRevision=e.operators.revision,t.operators.rebase(e.operators.members.map(e=>e.id)),t.mapping.rebase(e.work_dir.mapping??``)):this.sections.set(e.agent,this.build(e))}let n=[...this.sections.values()];this.edits.replace(n.flatMap(e=>[e.config,e.operators,e.mapping,e.doc])),await Promise.all(n.map(e=>e.view.doc.exists&&!e.doc.view?e.doc.load():Promise.resolve()));let r=this.route.agent,i=r&&this.sections.has(r)?r:t.agents[0]?.agent;i&&!this.active?this.activate(i):this.preview.request(),this.loadError=``}catch(e){this.loadError=o(e)}this.loading=!1,this.requestUpdate();let t=this.route.agent;t&&!this.scrolled&&(this.scrolled=!0,this.updateComplete.then(()=>document.getElementById(`a-${t}`)?.scrollIntoView({block:`start`})))}activate(e){this.active!==e&&(this.active=e,this.preview.setScope({level:`chat`,agent:e,chat_id:this.chatId}),this.requestUpdate())}async setMentionless(t,n){let r=t.view.agent;t.busy=!0,t.job=n?{job:``,status:`pending`,message:e(`d482bd2`)}:null,this.requestUpdate();try{let i=await this.call(`chats.set_mentionless`,{agent:r,chat_id:this.chatId,enabled:n});t.job=i,this.requestUpdate();let a=Date.now()+3e4;for(;i.status===`pending`&&Date.now()<a;)await new Promise(e=>setTimeout(e,800)),i=await this.call(`chats.mentionless_job`,{job:i.job}),t.job=i,this.requestUpdate();i.status===`pending`&&(t.job={...i,status:`failed`,message:e(`d659fca`)}),i.status===`done`&&h(e(n?`d33abb4`:`d802b86`),`ok`),await this.refresh(),n||(t.job=null)}catch(e){t.job={job:``,status:`failed`,message:o(e)}}t.busy=!1,this.requestUpdate()}async clearOverride(t){if(await u({title:e(`d3688a1`),body:e(`db3a665`,[t.view.work_dir.mapping?e(`d881f54`):e(`d778e74`)]),confirm:e(`dbce237`)})){try{t.view=await this.call(`chats.set_work_dir`,{agent:t.view.agent,chat_id:this.chatId,revision:t.opRevision,mapping:t.mapping.base.trim()||null,clear_override:!0}),t.opRevision=t.view.operators.revision,h(e(`d1a6f57`),`ok`)}catch(r){r instanceof n&&r.code===`conflict`?(await this.refetch(t),h(e(`d302143`),`warn`)):h(e(`d055d4f`,[o(r)]),`error`)}this.requestUpdate()}}block(e,t,n){return r`<section class="group">
      <header class="group-head">
        <div>
          <h3>${e}</h3>
          ${n?r`<p>${n}</p>`:i}
        </div>
      </header>
      ${t}
    </section>`}readOnlyReason(t){let n=t.view.config.values;return!n.length||n.some(e=>e.editable)?null:n.find(e=>e.deny_reason)?.deny_reason??e(`d5f24a8`)}switches(e){let n={draft:e.config,level:`chat`,preview:this.active===e.view.agent?this.preview:null,quietLock:!!this.readOnlyReason(e)},i=e.view.config.values.map(e=>e.key).map(e=>this.fields.get(e)).filter(e=>!!e);return i.length?r`<div class="card">${i.map(e=>w(e,n))}</div>`:r`<div class="card"><div class="empty"><p>${t(`d32cdc4`)}</p></div></div>`}mentionless(n){let a=n.view.mentionless,o=n.job,c=a.enabled?a.can_disable:a.can_enable;return r`<div class="row row--inline ${!c&&!n.busy?`is-disabled`:``}">
        <div class="row-grid">
          <div class="row-head">
            <div class="row-title"><span class="row-label">${t(`d40a899`)}</span></div>
            <span class="row-key">feishu.chat_reply_without_mention</span>
            <p class="row-desc">${t(`d47c4ed`)}</p>
            <div class="row-meta"><span class="chip chip-quiet">${t(`d0d8877`)}</span></div>
          </div>
          <div class="row-ctl">
            ${b({checked:a.enabled,label:e(`d40a899`),disabled:!c||n.busy,onChange:e=>this.setMentionless(n,e)})}
          </div>
        </div>
        ${a.reason&&!a.enabled&&!o?r`<p class="row-note">${s(`info`,`icon-sm`)}<span>${d(a.reason)}</span></p>`:i}
        ${o?r`<div class="job ${o.status}" role="status">
              ${o.status===`pending`?s(`loaderCircle`,`icon-sm spin`):o.status===`done`?s(`circleCheck`,`icon-sm`):s(`circleAlert`,`icon-sm`)}
              <span>
                ${o.status===`pending`?e(`d672fe0`):``}${o.message??``}
                ${o.apply_url?r`<a class="link-btn" href=${o.apply_url} target="_blank" rel="noopener noreferrer">${t(`da342d5`,[s(`externalLink`,`icon-sm`)])}</a>`:i}
              </span>
            </div>`:i}
      </div>`}operators(n){let a=n.view.operators,o=n.operators.dirtyCount()>0;return r`<div class="row row--block ${o?`is-changed`:``} ${a.editable?``:`is-disabled`}">
        <div class="row-grid">
          <div class="row-head">
            <div class="row-title"><span class="row-label">${t(`de3137d`)}</span></div>
            <span class="row-key">feishu.chat_operators.${this.chatId}</span>
            <p class="row-desc">${t(`dc507a8`,[this.agentName(n.view.agent)])}</p>
          </div>
          <div class="row-ctl">
            <sm-tags
              .values=${n.operators.value}
              .members=${a.members}
              .label=${e(`de3137d`)}
              ?disabled=${!a.editable}
              placeholder="${e(`d083393`)}"
              @change=${e=>n.operators.set(e.detail)}
            ></sm-tags>
            ${!a.editable&&a.reason?r`<span class="row-lock">${s(`lock`,`icon-sm`)}<span>${a.reason}</span></span>`:i}
          </div>
        </div>
        ${n.operators.error?r`<p class="row-error">${s(`circleAlert`,`icon-sm`)}<span>${n.operators.error}</span></p>`:i}
      </div>`}workDir(n){let a=n.view.work_dir,o=n.mapping.dirtyCount()>0;return r`<div class="card">
      <div class="row">
        <dl class="kv-list">
          <dt>${t(`d7ed094`)}</dt>
          <dd class="wd-path">${a.effective??e(`d2f5f1d`)}</dd>
          <dt>${t(`da488e9`)}</dt>
          <dd>${a.source?O[a.source]:`—`}</dd>
        </dl>
        ${a.live||a.reason?r`<p class="row-note">${s(`info`,`icon-sm`)}<span>${a.reason??e(`df6754e`)}</span></p>`:i}
      </div>
      <div class="row ${o?`is-changed`:``} ${a.editable?``:`is-disabled`}">
        <div class="row-grid">
          <div class="row-head">
            <div class="row-title"><span class="row-label">${t(`d83d89d`)}</span></div>
            <span class="row-key">runtime.chat_working_dirs.${this.chatId}</span>
            <p class="row-desc">${t(`d2869c8`)}</p>
          </div>
          <div class="row-ctl">
            ${x({value:n.mapping.value,label:e(`db27aea`),mono:!0,wide:!0,placeholder:`~/workspace/…`,disabled:!a.editable,onChange:e=>n.mapping.set(e)})}
            ${o?r`<span class="row-orig">${t(`dc2daf0`)} <s>${n.mapping.base||e(`de026c6`)}</s></span>`:i}
          </div>
        </div>
        ${n.mapping.error?r`<p class="row-error">${s(`circleAlert`,`icon-sm`)}<span>${n.mapping.error}</span></p>`:i}
      </div>
      ${a.override?r`<div class="row">
            <div class="row-grid">
              <div class="row-head">
                <div class="row-title"><span class="row-label">${t(`d5a5872`)}</span></div>
                <p class="row-desc wd-path">${a.override}</p>
              </div>
              <div class="row-ctl">
                <button class="btn btn-sm" ?disabled=${!a.editable} @click=${()=>this.clearOverride(n)}>${t(`d14266a`)}</button>
              </div>
            </div>
          </div>`:i}
    </div>`}doc(n){let a=n.doc;return n.docOpen?r`<div class="card card-pad">
      ${a.view?r`<div class="editor-head"><span class="editor-path">${a.view.path}</span>${a.view.exists?i:r`<span class="chip">${t(`dd9abaa`)}</span>`}</div>
            ${S({text:a.text,label:e(`d08d3e1`),readonly:!a.view.editable,placeholder:e(`d40e9ed`),onInput:e=>a.setText(e)})}
            ${a.error?r`<p class="row-error">${s(`circleAlert`,`icon-sm`)}${a.error}</p>`:i}`:a.loadError?r`<p class="row-error">${a.loadError}</p>`:r`<span class="sk sk-w1"></span>`}
    </div>`:r`<div class="card">
        <div class="empty">
          <h3>${t(`d1a9b25`)}</h3>
          <p>${t(`d642c47`,[this.agentName(n.view.agent)])}</p>
          <div class="empty-actions">
            <button
              class="btn"
              @click=${async()=>{n.docOpen=!0,a.view||await a.load(),this.requestUpdate()}}
            >
              ${t(`d1ab0e4`,[s(`plus`,`icon-sm`)])}
            </button>
          </div>
        </div>
      </div>`}schedules(e){let n=e.view.schedules;if(!n.length)return r`<div class="card"><div class="empty"><p>${t(`d09011e`,[this.agentName(e.view.agent)])}</p></div></div>`;let i=t=>{e.view={...e.view,schedules:e.view.schedules.map(e=>e.id===t.id?t:e)},this.requestUpdate()};return r`<div class="card">
      ${[...n].sort((e,t)=>Number(t.enabled)-Number(e.enabled)).map(e=>T(e,{onToggle:async(e,t)=>{let n=await E(this.call,e,t);n&&i(n)},onEdit:async e=>{let t=await D(this.call,e);t&&i(t)}}))}
    </div>`}section(n){let a=n.view,o=this.agentName(a.agent),c=a.config.values.filter(e=>e.value!==null).length+a.operators.members.length+ +!!a.work_dir.override,l=[a.work_dir.live?e(`d7d817d`):e(`dc1f158`),c?e(`ded467e`,[c]):e(`d23590d`),a.mentionless.enabled?e(`d3015b6`):``].filter(Boolean).join(` · `);return r`<section
      class="agent-section"
      id=${`a-${a.agent}`}
      aria-labelledby=${`ah-${a.agent}`}
      @mouseenter=${()=>this.activate(a.agent)}
      @focusin=${()=>this.activate(a.agent)}
    >
      <header class="agent-section-head">
        <span class="avatar avatar-lg" aria-hidden="true">${[...o][0]}</span>
        <div>
          <h2 id=${`ah-${a.agent}`}>${o} <span class="mono muted">${a.agent}</span></h2>
          <p>${l}</p>
        </div>
      </header>
      ${this.readOnlyReason(n)?r`<p class="row-note section-note">${s(`lock`,`icon-sm`)}<span>${t(`dce6171`,[d(this.readOnlyReason(n)??``)])}</span></p>`:i}
      ${this.block(e(`d1a37d0`),this.switches(n),e(`dd38b2e`))}
      ${this.detail?.kind===`group`?this.block(e(`debcff4`),r`<div class="card">${this.mentionless(n)}${this.operators(n)}</div>`):i}
      ${this.block(e(`d3db7b0`),this.workDir(n))} ${this.block(e(`d032d5c`),this.doc(n),e(`de3a102`))}
      ${this.block(e(`dac7cc1`),this.schedules(n))}
    </section>`}render(){let n=this.detail,a=!n||!!n.name,o=n?a?c(n):y(n.chat_id,{agent:this.active}):e(`d9fca8b`),u=m(o,r`${a?i:r`<span>${t(`d6fd1f1`)} </span>`}<button
        class="copy-id"
        title="${e(`da74d5a`)}"
        @click=${async()=>{try{await navigator.clipboard.writeText(this.chatId),h(e(`d024ad8`),`ok`,1600)}catch{h(e(`d5fe28d`),`warn`)}}}
      >
        ${a?this.chatId:e(`da74d5a`)}${s(`copy`,`icon-sm`)}
      </button>`,void 0,{href:`/chats`,label:e(`d35b49e`)}),d=this.loadError&&!n?_(e(`d6657ea`),this.loadError,()=>this.refresh()):n?r`${[...this.sections.values()].map(e=>this.section(e))}`:l(6);return this.withPreview(u,r`${this.conflictBanner()}${d}${this.saveBar()}`,r`<sm-preview .ctl=${this.preview} .botName=${this.agentName(this.active)}></sm-preview>`)}};customElements.define(`sm-page-chat`,k);