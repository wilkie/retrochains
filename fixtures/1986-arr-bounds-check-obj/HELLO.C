int main(void) {
  int a[5];
  int i = 2;
  int v;
  if (i >= 0 && i < 5) {
    v = (a[i] = 42);
  } else {
    v = -1;
  }
  return v;
}
