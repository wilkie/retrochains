int arr[5];
void recv(int *p);

void driver(int i) {
  recv(&arr[i]);
}
