int act(int x) {
  int s;
  s = 0;
  switch (x) {
    case 1: s = s + 10;
    case 2: s = s + 20;
    case 3: s = s + 30;
  }
  return s;
}
