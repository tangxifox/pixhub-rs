(function() {
  function getPreferredTheme() {
    var saved = localStorage.getItem('theme');
    if (saved === 'light' || saved === 'dark') return saved;
    return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
  }

  function applyTheme(theme) {
    document.documentElement.setAttribute('data-theme', theme);
  }

  var currentTheme = getPreferredTheme();
  applyTheme(currentTheme);

  var toggle = document.getElementById('themeToggle');
  if (toggle) {
    toggle.addEventListener('click', function() {
      var next = document.documentElement.getAttribute('data-theme') === 'dark' ? 'light' : 'dark';
      applyTheme(next);
      localStorage.setItem('theme', next);
    });
  }
})();

function copy(b) {
  var u = b.parentElement.getAttribute('data-url');
  navigator.clipboard.writeText(u).then(function() {
    b.textContent = '已复制';
    b.classList.add('copied');
    setTimeout(function() {
      b.textContent = '复制';
      b.classList.remove('copied');
    }, 1500);
  });
}
