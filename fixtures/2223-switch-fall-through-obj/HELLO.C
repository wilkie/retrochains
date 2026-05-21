int main(void) {
  int x = 1;
  int r = 0;
  switch (x) {
    case 1: r += 10;
    case 2: r += 20;
    case 3: r += 30;
  }
  return r;
}
