int check(int x) { return x > 0; }
int main(void) {
  int r;
  if (check(5)) {
    r = 100;
  } else {
    r = 200;
  }
  return r;
}
