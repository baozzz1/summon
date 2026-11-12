import{a as e,r as t}from"./i18n-YIZ94PuL.js";import{B as n,L as r,P as i,V as a,m as o,w as s}from"./router-DmWf1zNH.js";import{t as c}from"./live-D0VeMPic.js";import{l,o as u}from"./page-BvfONVkV.js";import{t as d}from"./ident-Bw3GTApD.js";import{i as f,o as p}from"./controls-DMP3WW_j.js";import{i as m}from"./turn-FLcJ5dAu.js";var h=[{value:`progress`,get label(){return e(`d2ee451`)}},{value:`result`,get label(){return e(`d36b1b0`)}}],g=[{value:`resume`,get label(){return e(`d2abac2`)}},{value:`fresh`,get label(){return e(`d964777`)}}],_=(e,t)=>e.find(e=>e.value===t)?.label??t;function v(e){if(e.kind===`cron`)return`cron ${e.spec}`;if(e.kind===`interval`){let t=Number(e.spec);return t%86400==0?`every ${t/86400}d`:t%3600==0?`every ${t/3600}h`:t%60==0?`every ${t/60}m`:`every ${t}s`}return e.kind===`once`?`at ${e.spec.slice(0,16)}`:e.spec}function y(n,a){let c=[];return n.enabled&&c.push(r`${t(`d6b88c5`)} <span class="tnum">${n.next_fire?o(n.next_fire):`—`}</span>`),c.push(r`${t(`d2b9bbc`)} <span class="tnum">${n.last_fired?o(n.last_fired):e(`da81b68`)}</span>`),c.push(_(h,n.output)),c.push(_(g,n.context)),a.agentLabel&&c.push(a.agentLabel),a.showChat&&n.chat_name&&c.push(n.chat_name),r`<div class="sched ${n.enabled?``:`is-off`}" id=${`s-${n.id}`}>
    <div class="sched-switch">
      ${p({checked:n.enabled,label:`${n.enabled?e(`d4e6fd0`):e(`ddced8a`)} ${n.human}`,disabled:!n.editable||a.busy,onChange:e=>a.onToggle(n,e)})}
    </div>
    <div class="sched-main">
      <div class="sched-title">
        <span class="sched-human">${n.human}</span>
        <span class="mono muted">#${n.id}</span>
        ${n.enabled?i:r`<span class="chip chip-quiet">${t(`d82395d`)}</span>`}
      </div>
      <p class="sched-prompt" title=${n.prompt}>${n.prompt}</p>
      <div class="sched-facts">${c.map((e,t)=>r`${t?r`<span class="meta-sep" aria-hidden="true"></span>`:i}<span>${e}</span>`)}</div>
      ${a.onRuns?b(n,a.runs,a.onRuns):i}
    </div>
    <div class="sched-actions">
      <button class="btn btn-sm" aria-label=${e(`d4ff722`,[n.id])} ?disabled=${!n.editable||a.busy} @click=${()=>a.onEdit(n)}>
        ${s(`pencil`,`icon-sm`)}<span class="btn-label">${t(`d051836`)}</span>
      </button>
    </div>
  </div>`}function b(e,n,a){let o=i;return n?.open&&(n.loading?o=r`<p class="muted">${t(`d4927a5`)}</p>`:n.error?o=r`<p class="muted">${t(`de86014`,[n.error])}</p>`:n.turns?.length?o=r`${n.turns.map(e=>m(e))}`:n.turns&&(o=r`<p class="muted">${t(`de12d88`)}</p>`)),r`<details
    class="sched-runs"
    .open=${c(!!n?.open)}
    @toggle=${t=>{let r=t.target.open;r!==!!n?.open&&a(e,r)}}
  >
    <summary>${t(`d230c85`,[s(`chevronRight`,`icon-chev`)])}</summary>
    <div class="sched-runs-body">${o}</div>
  </details>`}async function x(t,n,r){try{let i=await t(`schedules.set_enabled`,{id:n.id,enabled:r});return u(e(r?`d47b29d`:`dbe6c8c`,[n.id]),`ok`),i}catch(t){return u(e(`d860480`,[a(t)]),`error`),null}}function S(p,m){let _=v(m),y={when:_,prompt:m.prompt,output:m.output,context:m.context,check:null,checking:!1,saving:!1,error:``,errorKey:``},b,x=0;return l((l,v)=>{let S=()=>{b&&clearTimeout(b),b=setTimeout(async()=>{let e=++x;y.checking=!0,v.update();try{let t=await p(`schedules.check_when`,{when:y.when});e===x&&(y.check=t)}catch(t){e===x&&(y.check={ok:!1,next:[],error:a(t)})}e===x&&(y.checking=!1),v.update()},250)};!y.check&&!y.checking&&!b&&S();let C=async()=>{let t={};if(y.when.trim()!==_&&(t.when=y.when.trim()),y.prompt!==m.prompt&&(t.prompt=y.prompt),y.output!==m.output&&(t.output=y.output),y.context!==m.context&&(t.context=y.context),!Object.keys(t).length)return l(m);y.saving=!0,y.error=``,v.update();try{let n=await p(`schedules.edit`,{id:m.id,edit:t});u(e(`deda078`,[m.id]),`ok`),l(n)}catch(e){y.error=a(e),y.errorKey=e instanceof n?e.key??``:``,y.saving=!1,v.update()}},w=y.check,T=w&&!w.ok;return r`<div class="dlg-head">
          <h2>${t(`d6d9314`)} <span class="mono muted">#${m.id}</span></h2>
          <p>${t(`d98b6b0`,[m.chat_name??m.chat_id,d(m.created_by,{agent:m.agent})])}</p>
        </div>
        <div class="dlg-body">
          <div class="form-grid">
            <label class="form-field">
              <span class="form-label">${t(`d42b1af`)}</span>
              <input
                class="field-input mono"
                type="text"
                spellcheck="false"
                autocomplete="off"
                .value=${c(y.when)}
                @input=${e=>{y.when=e.target.value,S()}}
              />
              ${y.checking&&!w?r`<span class="form-hint">${t(`d84717e`)}</span>`:w?.ok?r`<span class="form-hint">${w.human}${w.next.length?e(`dfc732b`):e(`dc967bb`)}</span>
                      ${w.next.length?r`<span class="next-fires">${w.next.map(e=>r`<span class="chip tnum">${o(e)}</span>`)}</span>`:i}`:T?r`<span class="form-hint err">${w.error}</span>${w.forms?r`<span class="form-hint mono">${w.forms}</span>`:i}`:i}
            </label>
            <label class="form-field">
              <span class="form-label">${t(`d4b47db`)}</span>
              <textarea
                class="field-input"
                rows="4"
                .value=${c(y.prompt)}
                @input=${e=>{y.prompt=e.target.value,v.update()}}
              ></textarea>
            </label>
            <div class="form-field">
              <span class="form-label">${t(`de32e26`)}</span>
              ${f({label:e(`de32e26`),value:y.output,options:h,onChange:e=>{y.output=e,v.update()}})}
            </div>
            <div class="form-field">
              <span class="form-label">${t(`d6fc38e`)}</span>
              ${f({label:e(`d6fc38e`),value:y.context,options:g,onChange:e=>{y.context=e,v.update()}})}
              <span class="form-hint">${t(`d31b731`)}</span>
            </div>
            ${y.error?r`<p class="row-error" role="alert">${s(`circleAlert`,`icon-sm`)}<span>${y.error}</span></p>`:i}
          </div>
        </div>
        <div class="dlg-foot">
          <button class="btn" @click=${()=>v.close()}>${t(`d2cd0f3`)}</button>
          <button class="btn btn-primary" ?disabled=${y.saving||!!T||!y.prompt.trim()} @click=${C}>
            ${y.saving?e(`dff509c`):e(`da3030b`)}
          </button>
        </div>`},{label:e(`dd6902b`,[m.id])})}export{y as n,x as r,S as t};