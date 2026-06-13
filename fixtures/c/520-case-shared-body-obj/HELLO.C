int main(void) {
  int x;
  x = 2;
  switch (x) {
    case 1:
    case 2:
      return 100;
    case 3:
      return 200;
  }
  return 0;
}
