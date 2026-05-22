int sum3(int *a) { return a[0] + a[1] + a[2]; }
int main(void) {
  int data[3];
  data[0] = 10;
  data[1] = 20;
  data[2] = 30;
  return sum3(data);
}
