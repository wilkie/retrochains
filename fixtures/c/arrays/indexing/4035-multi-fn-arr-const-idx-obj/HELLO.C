int first_two(int *a) {
  return a[0] + a[1];
}
int main(void) {
  int data[3] = { 10, 20, 30 };
  return first_two(data);
}
