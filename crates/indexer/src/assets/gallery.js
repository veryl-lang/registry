const q=document.getElementById('q');
const pkgs=[...document.querySelectorAll('.pkg')];
const repos=[...document.querySelectorAll('.repo')];
const noresult=document.getElementById('noresult');
let cat='';
function filter(){
  const t=q.value.trim().toLowerCase();let any=false;
  for(const p of pkgs){
    const okText=!t||p.dataset.search.includes(t);
    const okCat=!cat||(' '+p.dataset.cats+' ').includes(' '+cat+' ');
    const show=okText&&okCat;p.classList.toggle('hidden',!show);if(show)any=true;
  }
  for(const r of repos){r.classList.toggle('hidden',!r.querySelector('.pkg:not(.hidden)'));}
  if(noresult)noresult.classList.toggle('hidden',any||(!t&&!cat));
}
function selectCat(c){cat=c;document.querySelectorAll('.catf').forEach(function(x){x.classList.toggle('active',x.dataset.cat===c);});filter();}
q.addEventListener('input',filter);
document.querySelectorAll('.catf,.cat').forEach(function(b){b.addEventListener('click',function(){selectCat(b.dataset.cat);});});
document.querySelectorAll('.copy').forEach(function(b){b.addEventListener('click',function(){navigator.clipboard.writeText(b.previousElementSibling.textContent).then(function(){b.textContent='Copied';setTimeout(function(){b.textContent='Copy';},1200);});});});
