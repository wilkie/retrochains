int main(void) {
  int x = 2;
  switch (x) {
    case 1:
    case 2:
    case 3:
      return 10;
    case 4:
    case 5:
      return 20;
  }
  return 0;
}
