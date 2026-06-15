int a[3];
int main(void) {
  int i = 1;
  a[0] = 10;
  a[1] = 20;
  a[2] = 30;
  return *(a + i);
}
