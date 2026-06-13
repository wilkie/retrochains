int a[3];
int main(void) {
  int i = 0;
  a[0] = 1;
  a[1] = 2;
  a[2] = 0;
  while (a[i]) i++;
  return i;
}
