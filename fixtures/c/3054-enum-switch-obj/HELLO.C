enum { ZERO, ONE, TWO };
int act(int x) {
  switch (x) {
    case ZERO: return 100;
    case ONE: return 200;
    case TWO: return 300;
  }
  return -1;
}
