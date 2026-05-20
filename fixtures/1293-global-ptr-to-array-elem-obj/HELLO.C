int arr[3];
int *p = &arr[1];
int main(void) {
  arr[0] = 10;
  arr[1] = 20;
  arr[2] = 30;
  return *p;
}
